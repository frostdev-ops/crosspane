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
#[cfg(all(windows, not(test)))]
pub(crate) use adapter::{
    ColdNamespaceAdmission, FirstInstallMutationPermit, FirstInstallReservation,
};
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
    ArchivedFirstHistory, FirstInstallArchiveResult, FirstRecoveryReservation, LogonArchiveResult,
    LogonReservation, OwnedArchiveResult, PriorLogonDisposition, RepairArchiveResult,
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
pub(crate) use adapter::{RepairTaskBinding, payload_repair_io::RepairFixedPayload};

#[cfg(all(windows, not(test)))]
pub(crate) use adapter::keeper;

#[cfg(all(windows, not(test)))]
pub(crate) use adapter::payload_repair_io::keeper as payload_repair_keeper;

#[cfg(all(windows, not(test)))]
pub(crate) use adapter::payload_repair_io::{
    NativePayloadRepairSelection, RepairCompletionAdmission, RepairKeeperSelection,
    RepairMutationPermit, RepairPeerImage,
};

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
    /// A distinct repair archive capability minted only from the genuine new task claim,
    /// independent four-image admission and retained exclusive namespace. Never from old bytes.
    #[cfg(not(test))]
    pub(crate) struct RepairArchiveResult {
        io: Arc<WindowsNativeIo>,
        namespace: Arc<super::supervisor_owner::ExclusiveSupervisorLease>,
        permit: Arc<super::super::service::task::TaskRunPermit>,
        archived: ArchivedEpochFiles,
    }
    #[cfg(not(test))]
    impl RepairArchiveResult {
        fn reverify_archive(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &Arc<super::super::service::task::TaskRunPermit>,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref())
                || !Arc::ptr_eq(permit, &self.permit)
                || !Arc::ptr_eq(&self.namespace, &owner.exclusive_lease(proof, deadline)?)
                || !permit.is_repair()
            {
                return Err(NativeError::Foreign);
            }
            io.verify_stop_lock(proof, lock, deadline)?;
            self.namespace.reverify(io, proof, deadline)?;
            permit.reverify(io, proof, deadline)?;
            let claim = EpochClaim::Repair(permit);
            let provenance = preparing_matches(io, proof, &claim.epoch(io)?, deadline)?;
            let intent = read_archive_intent(io, proof, deadline)?.ok_or(NativeError::Foreign)?;
            intent.require_complete()?;
            if intent != self.archived.intent
                || intent.operation != permit.operation()
                || intent.owner_creation != permit.owner_identity().creation()
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
                || source.bytes() != self.archived.bytes
            {
                return Err(NativeError::Foreign);
            }
            let prior = super::super::service::journal::Journal::decode(source.bytes())?;
            provenance.matches_history(intent.slot, &prior, intent.source, intent.sha256)?;
            super::activation::correlate_repair(
                permit.operation(),
                permit.user(),
                permit.repair_selection()?,
                &prior,
            )?;
            deadline.check()
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &Arc<super::super::service::task::TaskRunPermit>,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.reverify_archive(io, proof, lock, permit, owner, deadline)?;
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
            deadline.check()
        }
        pub(crate) fn publish_bound(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &Arc<super::super::service::task::TaskRunPermit>,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            // Running already exists here. Recheck the exact completed archive files and
            // Preparing provenance under this SAME lock without the pre-Create absence test.
            self.reverify_archive(io, proof, lock, permit, owner, deadline)?;
            publish_epoch_bound(
                io,
                proof,
                lock,
                &self.namespace,
                EpochClaim::Repair(permit),
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
        Repair(&'a Arc<super::super::service::task::TaskRunPermit>),
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
                Self::Repair(permit) => {
                    if !permit.is_repair() {
                        return Err(NativeError::Foreign);
                    }
                    permit.reverify(io, proof, deadline)
                }
                Self::Logon(permit) => permit.reverify(io, proof, deadline),
            }
        }
        fn epoch(&self, io: &WindowsNativeIo) -> NativeResult<super::activation::EpochProvenance> {
            match self {
                Self::Task(permit) | Self::Repair(permit) => {
                    super::activation::EpochProvenance::new(
                        permit.registration(),
                        permit.operation(),
                        &io.context.target.identity,
                        permit.owner_identity().pid(),
                        permit.owner_identity().creation(),
                    )
                }
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
                Self::Task(permit) | Self::Repair(permit) => permit.operation(),
                Self::Logon(permit) => permit.operation(),
            }
        }
        fn owner_creation(&self) -> u64 {
            match self {
                Self::Task(permit) | Self::Repair(permit) => permit.owner_identity().creation(),
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
                Self::Repair(permit) => {
                    if !permit.is_repair() {
                        return Err(NativeError::Foreign);
                    }
                    super::activation::correlate_repair(
                        permit.operation(),
                        permit.user(),
                        permit.repair_selection()?,
                        prior,
                    )?;
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
    // A6 fixed metadata retirement/publication region. No Stop, Run or executable effects.
    #[cfg(not(test))]
    mod repair_archive {
        use super::super::super::{
            payload::recovery::{
                self, FileRecordStamp, FileRecoveryCursor, FileRecoveryJournal, FileStamp,
                OperationRecord, OuterUpgradeRecord, StageCatalog,
            },
            removal::{RemovalCursor, RemovalRecord},
            repair::{
                self, ArchiveObservation, JournalObservation, RepairDebtObservation,
                RepairDiagnostic as Diagnostic, RepairOutcome, RepairPort, SourceKind,
                SourceSnapshot,
                record::{EvidenceIndex, EvidencePhase, RepairCursor, RepairRecord},
            },
        };
        use super::*;
        use aws_lc_rs::digest::{SHA256, digest};
        use records::{
            RepairPublicationIntent as PublicationIntent,
            RepairPublicationObservation as PublicationObservation,
            RepairPublicationPhase as PublicationPhase, RepairPublicationStamp as PublicationStamp,
            RepairPublicationTarget as PublicationTarget,
        };
        static TERMINAL_OWNER: std::sync::OnceLock<Arc<CallOwner>> = std::sync::OnceLock::new();

        fn hash(bytes: &[u8]) -> [u8; 32] {
            let mut value = [0; 32];
            value.copy_from_slice(digest(&SHA256, bytes).as_ref());
            value
        }
        fn identity(stamp: FileStamp) -> FileIdentity {
            FileIdentity {
                volume: stamp.volume,
                file: stamp.file,
            }
        }
        fn stamp(id: FileIdentity) -> FileStamp {
            FileStamp {
                volume: id.volume,
                file: id.file,
            }
        }
        fn source_name(kind: SourceKind) -> records::RecordName {
            match kind {
                SourceKind::OuterUpgrade => records::RecordName::OuterUpgrade,
                SourceKind::FileRecovery => records::RecordName::FileRecovery,
                SourceKind::Removal => records::RecordName::Removal,
            }
        }
        fn metadata_error(error: NativeError) -> NativeError {
            match error {
                NativeError::Invalid | NativeError::Oversize => NativeError::OutcomeUnknown,
                other => other,
            }
        }
        fn read(
            parent: &Anchor,
            context: &Context,
            name: records::RecordName,
            budget: &Deadline,
        ) -> NativeResult<Option<(FileIdentity, Vec<u8>)>> {
            let value = parent
                .read_private(
                    &name.file_name()?,
                    &context.security,
                    files::MAX_RECORD_BYTES,
                    budget,
                )
                .map_err(metadata_error)?;
            if let Some((_, bytes)) = &value {
                records::validate_for(&name, bytes).map_err(metadata_error)?;
            }
            Ok(value)
        }
        fn pending(
            parent: &Anchor,
            context: &Context,
            budget: &Deadline,
        ) -> NativeResult<Option<(FileIdentity, Vec<u8>)>> {
            // Raw fixed pending contains the intent-selected target envelope. No generic pending receipt.
            parent.read_private(
                &records::RecordName::RepairPending.file_name()?,
                &context.security,
                files::MAX_RECORD_BYTES,
                budget,
            )
        }
        fn observed(
            value: &Option<(FileIdentity, Vec<u8>)>,
        ) -> NativeResult<Option<PublicationStamp>> {
            value
                .as_ref()
                .map(|(id, bytes)| PublicationStamp::new(*id, bytes))
                .transpose()
        }
        type DurablePublication = (FileIdentity, Vec<u8>, PublicationIntent);
        fn publication(
            parent: &Anchor,
            ctx: &Context,
            budget: &Deadline,
        ) -> NativeResult<(Option<DurablePublication>, PublicationObservation)> {
            let intent = read(
                parent,
                ctx,
                records::RecordName::RepairPublicationIntent,
                budget,
            )?;
            let pending = pending(parent, ctx, budget)?;
            match intent {
                None if pending.is_none() => Ok((None, PublicationObservation::Published)),
                None => Ok((None, PublicationObservation::Unknown)),
                Some((id, bytes)) => {
                    let intent = PublicationIntent::decode(&bytes).map_err(metadata_error)?;
                    let current = read(parent, ctx, intent.target().name(), budget)?;
                    if let Some((_, bytes)) = &pending {
                        records::validate_for(&intent.target().name(), bytes)
                            .map_err(metadata_error)?;
                    }
                    let state = records::recover_repair_publication(
                        &intent,
                        observed(&current)?,
                        observed(&pending)?,
                    );
                    Ok((Some((id, bytes, intent)), state))
                }
            }
        }
        fn clean_publication(
            parent: &Anchor,
            ctx: &Context,
            budget: &Deadline,
        ) -> NativeResult<()> {
            let (intent, state) = publication(parent, ctx, budget)?;
            if intent.is_none()
                && (read(parent, ctx, records::RecordName::Repair, budget)?.is_some()
                    || read(parent, ctx, records::RecordName::RepairEvidence, budget)?.is_some())
            {
                return Err(NativeError::OutcomeUnknown);
            }
            if state != PublicationObservation::Published
                || intent
                    .as_ref()
                    .is_some_and(|(_, _, i)| i.phase() != PublicationPhase::Published)
            {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(())
        }
        fn index(parent: &Anchor, ctx: &Context, budget: &Deadline) -> NativeResult<EvidenceIndex> {
            match read(parent, ctx, records::RecordName::RepairEvidence, budget)? {
                Some((_, bytes)) => EvidenceIndex::decode(&bytes).map_err(metadata_error),
                None => {
                    for slot in 0..3 {
                        if parent
                            .repair_evidence_slot(slot, false, &ctx.security, budget)?
                            .is_some()
                        {
                            return Err(NativeError::Foreign);
                        }
                    }
                    Ok(EvidenceIndex::default())
                }
            }
        }
        fn verify_evidence(
            parent: &Anchor,
            ctx: &Context,
            index: &EvidenceIndex,
            budget: &Deadline,
        ) -> NativeResult<()> {
            for slot in 0..3 {
                match index.get(slot) {
                    None => {
                        if parent
                            .repair_evidence_slot(slot, false, &ctx.security, budget)?
                            .is_some()
                        {
                            return Err(NativeError::Foreign);
                        }
                    }
                    Some(entry) => {
                        if entry.phase() != EvidencePhase::Complete {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        let destination = parent
                            .repair_evidence_slot(slot, false, &ctx.security, budget)?
                            .ok_or(NativeError::Foreign)?;
                        for source in entry.sources() {
                            let (id, bytes) = destination
                                .read_private(
                                    &PrivateName::new(source.kind().leaf())?,
                                    &ctx.security,
                                    files::MAX_RECORD_BYTES,
                                    budget,
                                )?
                                .ok_or(NativeError::Foreign)?;
                            records::validate_for(&source_name(source.kind()), &bytes)?;
                            if !matches(source, id, &bytes) {
                                return Err(NativeError::Foreign);
                            }
                        }
                    }
                }
            }
            budget.check()
        }
        /// Additive old-control exclusion. NoRepair keeps all previous authority/signatures.
        pub(super) fn refuse_active_repair(
            ctx: &Context,
            lease: &LockState,
            budget: &Deadline,
        ) -> NativeResult<()> {
            validate_payload_lock(ctx, lease, budget)?;
            let current = read(&lease.parent, ctx, records::RecordName::Repair, budget)?;
            if current.is_none() {
                if read(
                    &lease.parent,
                    ctx,
                    records::RecordName::RepairPublicationIntent,
                    budget,
                )?
                .is_some()
                    || pending(&lease.parent, ctx, budget)?.is_some()
                    || read(
                        &lease.parent,
                        ctx,
                        records::RecordName::RepairEvidence,
                        budget,
                    )?
                    .is_some()
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                index(&lease.parent, ctx, budget)?;
                return Ok(());
            }
            let (_, bytes) = current.ok_or(NativeError::Foreign)?;
            let record = RepairRecord::decode(&bytes)?;
            record.context().same_user(&ctx.target.identity)?;
            if record.cursor() != RepairCursor::Complete {
                return Err(NativeError::OutcomeUnknown);
            }
            clean_publication(&lease.parent, ctx, budget)?;
            let index = index(&lease.parent, ctx, budget)?;
            verify_evidence(&lease.parent, ctx, &index, budget)
        }
        impl WindowsNativeIo {
            pub(crate) fn refuse_unsettled_repair_readonly(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                let ctx = self.context.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    ctx.validate(&budget)?;
                    let Some(parent) =
                        Anchor::open(ctx.target.paths.installer(), &ctx.security, true, &budget)?
                    else {
                        return Ok(());
                    };
                    let current = read(&parent, &ctx, records::RecordName::Repair, &budget)?;
                    match current {
                        None => {
                            if read(
                                &parent,
                                &ctx,
                                records::RecordName::RepairPublicationIntent,
                                &budget,
                            )?
                            .is_some()
                                || pending(&parent, &ctx, &budget)?.is_some()
                                || read(
                                    &parent,
                                    &ctx,
                                    records::RecordName::RepairEvidence,
                                    &budget,
                                )?
                                .is_some()
                            {
                                return Err(NativeError::OutcomeUnknown);
                            }
                            index(&parent, &ctx, &budget)?;
                            Ok(())
                        }
                        Some((_, bytes)) => {
                            let record = RepairRecord::decode(&bytes).map_err(metadata_error)?;
                            record.context().same_user(&ctx.target.identity)?;
                            if record.cursor() != RepairCursor::Complete {
                                return Err(NativeError::OutcomeUnknown);
                            }
                            clean_publication(&parent, &ctx, &budget)?;
                            let index = index(&parent, &ctx, &budget)?;
                            verify_evidence(&parent, &ctx, &index, &budget)
                        }
                    }
                })
            }
            pub(crate) fn refuse_unsettled_repair(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                self.lock_binding(proof, lock, deadline)?;
                let ctx = self.context.clone();
                let lease = lock.0.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    refuse_active_repair(&ctx, &lease, &budget)
                })
            }
        }
        fn check_slot(
            parent: &Anchor,
            ctx: &Context,
            record: &RepairRecord,
            budget: &Deadline,
        ) -> NativeResult<()> {
            if record.plan().archives().is_empty() {
                return Ok(());
            }
            let slot = record.slot().ok_or(NativeError::Foreign)?;
            let index = index(parent, ctx, budget)?;
            let entry = index.get(slot).ok_or(NativeError::Foreign)?;
            if entry.repair() != record.operation() || entry.sources() != record.plan().archives() {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
        fn snapshot(
            kind: SourceKind,
            op: [u8; 16],
            id: FileIdentity,
            bytes: &[u8],
        ) -> NativeResult<SourceSnapshot> {
            SourceSnapshot::new(kind, op, stamp(id), hash(bytes), bytes.len() as u64)
        }
        fn matches(source: &SourceSnapshot, id: FileIdentity, bytes: &[u8]) -> bool {
            source.stamp() == stamp(id)
                && source.sha256() == hash(bytes)
                && source.len() == bytes.len() as u64
        }
        fn catalog(
            parent: &Anchor,
            ctx: &Context,
            budget: &Deadline,
        ) -> NativeResult<StageCatalog> {
            match read(parent, ctx, records::RecordName::StageCatalog, budget)? {
                Some((_, bytes)) => {
                    let value: StageCatalog =
                        records::record_data(&records::RecordName::StageCatalog, &bytes)?;
                    value.validate()?;
                    Ok(value)
                }
                None => Ok(StageCatalog::default()),
            }
        }
        fn copy_absent(
            ctx: &Context,
            operation: [u8; 16],
            removal: bool,
            budget: &Deadline,
        ) -> NativeResult<()> {
            let parent = if removal {
                Anchor::open(
                    &format!(
                        "{}\\Crosspane\\runtime\\removal\\{}",
                        ctx.target.paths.local(),
                        records::hex(&operation)
                    ),
                    &ctx.security,
                    true,
                    budget,
                )?
            } else {
                Anchor::open(ctx.target.paths.install(), &ctx.security, true, budget)?
                    .map(|p| p.child("payload-stage", &ctx.security, budget))
                    .transpose()?
                    .flatten()
                    .map(|p| p.child(&records::hex(&operation), &ctx.security, budget))
                    .transpose()?
                    .flatten()
            };
            if let Some(parent) = parent {
                for leaf in ["keeper-copy.exe", "helper-copy.exe"] {
                    if parent.opaque(leaf, false, &ctx.security, budget)?.is_some() {
                        return Err(NativeError::OutcomeUnknown);
                    }
                }
            }
            budget.check()
        }
        enum TerminalDocument {
            Outer(Box<OuterUpgradeRecord>),
            File(Box<FileRecoveryJournal>),
            Removal(Box<RemovalRecord>),
        }
        fn terminal_document(
            parent: &Anchor,
            ctx: &Context,
            kind: SourceKind,
            bytes: &[u8],
            budget: &Deadline,
        ) -> NativeResult<TerminalDocument> {
            match kind {
                SourceKind::OuterUpgrade => {
                    let outer = OuterUpgradeRecord::decode(bytes).map_err(metadata_error)?;
                    outer.context().same_user(&ctx.target.identity)?;
                    outer
                        .context()
                        .matches(&ctx.target.identity)
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    let (_, op) = read(
                        parent,
                        ctx,
                        records::RecordName::Operation(outer.operation()),
                        budget,
                    )?
                    .ok_or(NativeError::Foreign)?;
                    let operation: OperationRecord = records::record_data(
                        &records::RecordName::Operation(outer.operation()),
                        &op,
                    )?;
                    let catalog = catalog(parent, ctx, budget)?;
                    recovery::validate_outer_terminal_retirement(&outer, &operation, &catalog)?;
                    // A6 cannot clear a cancelled catalog or delete a copy to manufacture settlement.
                    if catalog.active.is_some() {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    copy_absent(ctx, outer.operation(), false, budget)?;
                    Ok(TerminalDocument::Outer(Box::new(outer)))
                }
                SourceKind::FileRecovery => {
                    let journal = FileRecoveryJournal::decode(bytes).map_err(metadata_error)?;
                    if journal.cursor() != FileRecoveryCursor::Retired {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    journal
                        .outer_snapshot()
                        .context()
                        .same_user(&ctx.target.identity)?;
                    let (op_id, op_bytes) = read(
                        parent,
                        ctx,
                        records::RecordName::Operation(journal.operation()),
                        budget,
                    )?
                    .ok_or(NativeError::Foreign)?;
                    let operation: OperationRecord = records::record_data(
                        &records::RecordName::Operation(journal.operation()),
                        &op_bytes,
                    )?;
                    journal.matches_sources(
                        journal.outer_snapshot(),
                        journal.outer_record(),
                        &operation,
                        FileRecordStamp::new(stamp(op_id), hash(&op_bytes))?,
                    )?;
                    if catalog(parent, ctx, budget)?.active.is_some()
                        || read(parent, ctx, records::RecordName::OuterUpgrade, budget)?.is_some()
                    {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    copy_absent(ctx, journal.operation(), false, budget)?;
                    Ok(TerminalDocument::File(Box::new(journal)))
                }
                SourceKind::Removal => {
                    let record = RemovalRecord::decode(bytes).map_err(metadata_error)?;
                    if record.cursor() != RemovalCursor::Retired {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    record.context().same_user(&ctx.target.identity)?;
                    record
                        .context()
                        .matches(&ctx.target.identity)
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    if catalog(parent, ctx, budget)?.active.is_some() {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    copy_absent(ctx, record.operation(), true, budget)?;
                    Ok(TerminalDocument::Removal(Box::new(record)))
                }
            }
        }
        fn source_operation(document: &TerminalDocument) -> [u8; 16] {
            match document {
                TerminalDocument::Outer(r) => r.operation(),
                TerminalDocument::File(r) => r.operation(),
                TerminalDocument::Removal(r) => r.operation(),
            }
        }
        fn diagnostic<T>(value: native::RepairRead<T>) -> Result<T, Diagnostic> {
            match value {
                native::RepairRead::Present(v) => Ok(v),
                native::RepairRead::Missing => Err(Diagnostic::Missing),
                native::RepairRead::AccessDenied => Err(Diagnostic::AccessDenied),
                native::RepairRead::Unsafe => Err(Diagnostic::UnsafeForeign),
                native::RepairRead::Unavailable => Err(Diagnostic::Unavailable),
                native::RepairRead::Unknown => Err(Diagnostic::Unknown),
            }
        }
        impl WindowsNativeIo {
            pub(crate) fn inspect_repair_debt(
                self: &Arc<Self>,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<RepairDebtObservation> {
                let budget = proof.budget(self, deadline)?;
                let ctx = self.context.clone();
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let parent = diagnostic(native::repair_readonly_diagnostic(|| {
                        ctx.validate(&budget)?;
                        Anchor::open(ctx.target.paths.installer(), &ctx.security, true, &budget)
                    }));
                    let parent = match parent {
                        Ok(p) => p,
                        Err(Diagnostic::Missing) => {
                            return RepairDebtObservation::new(
                                vec![
                                    JournalObservation::Absent(SourceKind::OuterUpgrade),
                                    JournalObservation::Absent(SourceKind::FileRecovery),
                                    JournalObservation::Absent(SourceKind::Removal),
                                ],
                                Diagnostic::Healthy,
                            );
                        }
                        Err(d) => return RepairDebtObservation::new(Vec::new(), d),
                    };
                    let publication: Result<EvidenceIndex, Diagnostic> =
                        diagnostic(native::repair_readonly_diagnostic(|| {
                            clean_publication(&parent, &ctx, &budget)?;
                            let index = index(&parent, &ctx, &budget)?;
                            verify_evidence(&parent, &ctx, &index, &budget)?;
                            if let Some((_, bytes)) =
                                read(&parent, &ctx, records::RecordName::Repair, &budget)?
                            {
                                let record =
                                    RepairRecord::decode(&bytes).map_err(metadata_error)?;
                                record.context().same_user(&ctx.target.identity)?;
                                if record.cursor() != RepairCursor::Complete {
                                    return Err(NativeError::OutcomeUnknown);
                                }
                                if record.slot().is_some() {
                                    check_slot(&parent, &ctx, &record, &budget)?;
                                }
                            } else if read(
                                &parent,
                                &ctx,
                                records::RecordName::RepairEvidence,
                                &budget,
                            )?
                            .is_some()
                            {
                                return Err(NativeError::OutcomeUnknown);
                            }
                            Ok(Some(index))
                        }));
                    let (publication, evidence) = match publication {
                        Ok(index) => (Diagnostic::Healthy, Some(index)),
                        Err(d) => (d, None),
                    };
                    let mut journals = Vec::with_capacity(3);
                    for kind in [
                        SourceKind::OuterUpgrade,
                        SourceKind::FileRecovery,
                        SourceKind::Removal,
                    ] {
                        let value = diagnostic(native::repair_readonly_diagnostic(|| {
                            let Some((id, bytes)) =
                                read(&parent, &ctx, source_name(kind), &budget)?
                            else {
                                return Ok(None);
                            };
                            let document = terminal_document(&parent, &ctx, kind, &bytes, &budget)?;
                            Ok(Some(snapshot(
                                kind,
                                source_operation(&document),
                                id,
                                &bytes,
                            )?))
                        }));
                        journals.push(match value {
                            Ok(source) => JournalObservation::Terminal(source),
                            Err(Diagnostic::Missing) => JournalObservation::Absent(kind),
                            Err(d) => JournalObservation::Retained(kind, d),
                        });
                    }
                    let publication = if publication == Diagnostic::Healthy
                        && journals
                            .iter()
                            .any(|j| matches!(j, JournalObservation::Terminal(_)))
                        && evidence
                            .as_ref()
                            .is_some_and(|i| (0..3).all(|s| i.get(s).is_some()))
                    {
                        Diagnostic::Unknown
                    } else {
                        publication
                    };
                    budget.check()?;
                    RepairDebtObservation::new(journals, publication)
                })
            }
        }

        pub(crate) struct RepairMutationPermit {
            io: Arc<WindowsNativeIo>,
            bytes: Vec<u8>,
            record: RepairRecord,
        }
        impl RepairMutationPermit {
            pub(crate) fn record(&self) -> &RepairRecord {
                &self.record
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                selection: &Arc<RepairSelection>,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref()) || self.record.plan() != selection.plan() {
                    return Err(NativeError::Foreign);
                }
                selection.reverify(io, proof, lock, deadline)?;
                let ctx = io.context.clone();
                let lease = lock.0.clone();
                let bytes = self.bytes.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Observation, deadline, move || {
                    validate_payload_lock(&ctx, &lease, &budget)?;
                    clean_publication(&lease.parent, &ctx, &budget)?;
                    let (_, actual) =
                        read(&lease.parent, &ctx, records::RecordName::Repair, &budget)?
                            .ok_or(NativeError::Foreign)?;
                    if actual != bytes {
                        return Err(NativeError::Foreign);
                    }
                    let record = RepairRecord::decode(&actual)?;
                    record.context().matches(&ctx.target.identity)?;
                    if record.slot().is_some() {
                        check_slot(&lease.parent, &ctx, &record, &budget)?;
                    }
                    budget.check()
                })
            }
        }
        fn persist_intent(
            parent: &Anchor,
            ctx: &Context,
            old: &mut Option<(FileIdentity, Vec<u8>, PublicationIntent)>,
            intent: PublicationIntent,
            budget: &Deadline,
            change: &Change,
        ) -> NativeResult<()> {
            let bytes = intent.encode()?;
            let previous = old.as_ref().map(|(id, b, _)| (*id, b.as_slice()));
            change.reached();
            let id =
                parent.write_repair_publication_intent(previous, &bytes, &ctx.security, budget)?;
            *old = Some((id, bytes, intent));
            Ok(())
        }
        fn publish_fixed(
            ctx: &Context,
            lease: &LockState,
            operation: [u8; 16],
            target: PublicationTarget,
            bytes: &[u8],
            budget: &Deadline,
            change: &Change,
        ) -> NativeResult<()> {
            validate_payload_lock(ctx, lease, budget)?;
            let parent = &lease.parent;
            if target == PublicationTarget::EvidenceIndex {
                index(parent, ctx, budget)?
                    .publication_successor(&EvidenceIndex::decode(bytes)?)?;
            }
            let (mut durable, state) = publication(parent, ctx, budget)?;
            let same = durable
                .as_ref()
                .is_some_and(|(_, _, i)| i.matches_request(operation, target, bytes));
            if same && state == PublicationObservation::Published {
                if durable
                    .as_ref()
                    .is_some_and(|(_, _, i)| i.phase() != PublicationPhase::Published)
                {
                    let mut i = durable.as_ref().ok_or(NativeError::Foreign)?.2.clone();
                    i.published()?;
                    persist_intent(parent, ctx, &mut durable, i, budget, change)?;
                }
                return Ok(());
            }
            if !same {
                if state != PublicationObservation::Published
                    || durable
                        .as_ref()
                        .is_some_and(|(_, _, i)| i.phase() != PublicationPhase::Published)
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                let current = read(parent, ctx, target.name(), budget)?;
                let intent = PublicationIntent::new(operation, target, observed(&current)?, bytes)?;
                persist_intent(parent, ctx, &mut durable, intent, budget, change)?;
            } else if state == PublicationObservation::Unknown {
                return Err(NativeError::OutcomeUnknown);
            }
            let mut intent = durable.as_ref().ok_or(NativeError::Foreign)?.2.clone();
            if intent.phase() == PublicationPhase::Preparing {
                if pending(parent, ctx, budget)?.is_some() {
                    return Err(NativeError::OutcomeUnknown);
                }
                change.reached();
                let id = parent.create_repair_pending(bytes, &ctx.security, budget)?;
                intent.pending_ready(PublicationStamp::new(id, bytes)?)?;
                persist_intent(parent, ctx, &mut durable, intent.clone(), budget, change)?;
            }
            if intent.phase() == PublicationPhase::PendingReady {
                intent.replace_intent()?;
                persist_intent(parent, ctx, &mut durable, intent.clone(), budget, change)?;
            }
            if intent.phase() != PublicationPhase::ReplaceIntent {
                return Err(NativeError::OutcomeUnknown);
            }
            let current = read(parent, ctx, target.name(), budget)?;
            let pending = pending(parent, ctx, budget)?;
            match records::recover_repair_publication(
                &intent,
                observed(&current)?,
                observed(&pending)?,
            ) {
                PublicationObservation::PendingReady => {
                    let id = intent.pending().ok_or(NativeError::Foreign)?.identity();
                    let previous = current.as_ref().map(|(id, b)| (*id, b.as_slice()));
                    change.reached();
                    parent.publish_repair_pending(
                        &target.name().file_name()?,
                        id,
                        bytes,
                        previous,
                        &ctx.security,
                        budget,
                    )?;
                }
                PublicationObservation::Published => {}
                _ => return Err(NativeError::OutcomeUnknown),
            }
            intent.published()?;
            persist_intent(parent, ctx, &mut durable, intent, budget, change)?;
            clean_publication(parent, ctx, budget)
        }
        fn requested_record(
            parent: &Anchor,
            ctx: &Context,
            requested: &RepairRecord,
            budget: &Deadline,
        ) -> NativeResult<()> {
            requested.context().matches(&ctx.target.identity)?;
            match read(parent, ctx, records::RecordName::Repair, budget)? {
                None => {
                    if requested.cursor() != RepairCursor::Selected || requested.slot().is_some() {
                        return Err(NativeError::Foreign);
                    }
                    let index = index(parent, ctx, budget)?;
                    verify_evidence(parent, ctx, &index, budget)?;
                }
                Some((_, old)) => {
                    let old = RepairRecord::decode(&old)?;
                    if old.cursor() == RepairCursor::Complete
                        && old.operation() != requested.operation()
                    {
                        old.context().same_user(&ctx.target.identity)?;
                        let index = index(parent, ctx, budget)?;
                        verify_evidence(parent, ctx, &index, budget)?;
                        if requested.cursor() != RepairCursor::Selected
                            || requested.slot().is_some()
                        {
                            return Err(NativeError::Foreign);
                        }
                    } else {
                        old.publication_successor(requested)?;
                    }
                }
            }
            Ok(())
        }
        fn publish_record(
            io: &Arc<WindowsNativeIo>,
            proof: &SupportProof,
            lock: &InstallerLock,
            selection: &Arc<RepairSelection>,
            record: &RepairRecord,
            deadline: &Deadline,
        ) -> NativeResult<Arc<RepairMutationPermit>> {
            selection.reverify(io, proof, lock, deadline)?;
            record.validate()?;
            if record.plan() != selection.plan() {
                return Err(NativeError::Foreign);
            }
            let ctx = io.context.clone();
            let lease = lock.0.clone();
            let budget = proof.budget(io, deadline)?;
            let requested = record.clone();
            let bytes = record.encode()?;
            let output = bytes.clone();
            let owner = io.owner.clone();
            io.owner.run(Dispatch::Mutation, deadline, move || {
                let change = Change::new();
                let result = change.finish((|| {
                    validate_payload_lock(&ctx, &lease, &budget)?;
                    requested_record(&lease.parent, &ctx, &requested, &budget)?;
                    publish_fixed(
                        &ctx,
                        &lease,
                        requested.operation(),
                        PublicationTarget::Repair,
                        &bytes,
                        &budget,
                        &change,
                    )
                })());
                if matches!(result, Err(NativeError::OutcomeUnknown)) {
                    owner.retire_mutations();
                }
                result
            })?;
            let permit = Arc::new(RepairMutationPermit {
                io: io.clone(),
                bytes: output,
                record: record.clone(),
            });
            let proof = io.admit_support(deadline)?;
            permit.reverify(io, &proof, lock, selection, deadline)?;
            Ok(permit)
        }
        fn reserve_index(
            io: &Arc<WindowsNativeIo>,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &Arc<RepairMutationPermit>,
            selection: &Arc<RepairSelection>,
            deadline: &Deadline,
        ) -> NativeResult<u8> {
            permit.reverify(io, proof, lock, selection, deadline)?;
            if permit.record.cursor() != RepairCursor::Selected || permit.record.slot().is_some() {
                return Err(NativeError::Foreign);
            }
            let ctx = io.context.clone();
            let lease = lock.0.clone();
            let budget = proof.budget(io, deadline)?;
            let record = permit.record.clone();
            let expected = permit.bytes.clone();
            let owner = io.owner.clone();
            io.owner.run(Dispatch::Mutation, deadline, move || {
                let change = Change::new();
                let result = change.finish((|| {
                    validate_payload_lock(&ctx, &lease, &budget)?;
                    clean_publication(&lease.parent, &ctx, &budget)?;
                    let (_, actual) =
                        read(&lease.parent, &ctx, records::RecordName::Repair, &budget)?
                            .ok_or(NativeError::Foreign)?;
                    if actual != expected {
                        return Err(NativeError::Foreign);
                    }
                    let mut index = index(&lease.parent, &ctx, &budget)?;
                    let slot = index.reserve(record.operation(), record.plan().archives())?;
                    publish_fixed(
                        &ctx,
                        &lease,
                        record.operation(),
                        PublicationTarget::EvidenceIndex,
                        &index.encode()?,
                        &budget,
                        &change,
                    )?;
                    Ok(slot)
                })());
                if matches!(result, Err(NativeError::OutcomeUnknown)) {
                    owner.retire_mutations();
                }
                result
            })
        }

        struct RemovalNamespace {
            lease: Arc<super::super::super::payload::helper::removal::RemovalKeeperLease>,
            _server: tokio::net::windows::named_pipe::NamedPipeServer,
            _runtime: tokio::runtime::Runtime,
        }
        enum Namespace {
            Keeper(Arc<keeper::RepairNamespaceHold>),
            Removal(Arc<RemovalNamespace>),
        }
        impl Namespace {
            fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                operation: [u8; 16],
                deadline: &Deadline,
            ) -> NativeResult<()> {
                match self {
                    Self::Keeper(n) => n.reverify(io, proof, deadline),
                    Self::Removal(n) => n.lease.reverify(io, proof, operation, deadline),
                }
            }
            fn bound(&self, ctx: &Context, budget: &Deadline) -> NativeResult<()> {
                match self {
                    Self::Keeper(n) => n.reverify_bound(ctx, budget),
                    Self::Removal(n) => {
                        use windows_sys::Win32::System::Pipes::{
                            GetNamedPipeInfo, PIPE_SERVER_END,
                        };
                        let mut flags = 0;
                        // SAFETY: the retained actual original removal FIRST_INSTANCE server duplicate,
                        // not a recorded handle value; read-only exact-object information query.
                        if unsafe {
                            GetNamedPipeInfo(
                                n.lease.retained_namespace().as_raw_handle(),
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
                        ctx.validate(budget)
                    }
                }
            }
        }
        struct FileAuthority {
            _seal: FileRecoverySeal,
            _root: FileRecoveryRoot,
        }
        pub(crate) struct RepairTerminal {
            io: Arc<WindowsNativeIo>,
            snapshot: SourceSnapshot,
            bytes: Vec<u8>,
            namespace: Arc<Namespace>,
            associated: Vec<(records::RecordName, Option<PublicationStamp>)>,
            _file: Option<FileAuthority>,
        }
        fn associated(
            parent: &Anchor,
            ctx: &Context,
            source: &SourceSnapshot,
            budget: &Deadline,
        ) -> NativeResult<Vec<(records::RecordName, Option<PublicationStamp>)>> {
            let names = match source.kind() {
                SourceKind::Removal => vec![records::RecordName::StageCatalog],
                _ => vec![
                    records::RecordName::Operation(source.source_operation()),
                    records::RecordName::StageCatalog,
                ],
            };
            names
                .into_iter()
                .map(|name| {
                    let value = read(parent, ctx, name.clone(), budget)?;
                    Ok((name, observed(&value)?))
                })
                .collect()
        }
        fn source_at(
            parent: &Anchor,
            ctx: &Context,
            record: &RepairRecord,
            source: &SourceSnapshot,
            budget: &Deadline,
        ) -> NativeResult<(ArchiveObservation, Option<Vec<u8>>)> {
            let actual = read(parent, ctx, source_name(source.kind()), budget)?;
            let destination = match record.slot() {
                Some(slot) => {
                    check_slot(parent, ctx, record, budget)?;
                    parent
                        .repair_evidence_slot(slot, false, &ctx.security, budget)?
                        .map(|p| {
                            p.read_private(
                                &PrivateName::new(source.kind().leaf())?,
                                &ctx.security,
                                files::MAX_RECORD_BYTES,
                                budget,
                            )
                        })
                        .transpose()?
                        .flatten()
                }
                None => None,
            };
            if actual
                .as_ref()
                .is_some_and(|(id, b)| !matches(source, *id, b))
                || destination
                    .as_ref()
                    .is_some_and(|(id, b)| !matches(source, *id, b))
            {
                return Ok((ArchiveObservation::Changed, None));
            }
            Ok(match (actual, destination) {
                (Some((_, b)), None) => (ArchiveObservation::SourceOnly, Some(b)),
                (None, Some((_, b))) => (ArchiveObservation::DestinationOnly, Some(b)),
                (Some(_), Some(_)) => (ArchiveObservation::Both, None),
                (None, None) => (ArchiveObservation::Neither, None),
            })
        }
        impl RepairTerminal {
            fn reverify(
                &self,
                io: &Arc<WindowsNativeIo>,
                proof: &SupportProof,
                lock: &InstallerLock,
                record: &RepairRecord,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !Arc::ptr_eq(io, &self.io) {
                    return Err(NativeError::Foreign);
                }
                self.namespace
                    .reverify(io, proof, self.snapshot.source_operation(), deadline)?;
                let ctx = io.context.clone();
                let lease = lock.0.clone();
                let budget = proof.budget(io, deadline)?;
                let source = self.snapshot.clone();
                let bytes = self.bytes.clone();
                let record = record.clone();
                let associated = self.associated.clone();
                let namespace = self.namespace.clone();
                io.owner.run(Dispatch::Observation, deadline, move || {
                    validate_payload_lock(&ctx, &lease, &budget)?;
                    namespace.bound(&ctx, &budget)?;
                    let (state, actual) =
                        source_at(&lease.parent, &ctx, &record, &source, &budget)?;
                    if !matches!(
                        state,
                        ArchiveObservation::SourceOnly | ArchiveObservation::DestinationOnly
                    ) || actual.as_deref() != Some(bytes.as_slice())
                    {
                        return Err(NativeError::Foreign);
                    }
                    terminal_document(&lease.parent, &ctx, source.kind(), &bytes, &budget)?;
                    for (name, expected) in associated {
                        if observed(&read(&lease.parent, &ctx, name, &budget)?)? != expected {
                            return Err(NativeError::Foreign);
                        }
                    }
                    budget.check()
                })
            }
        }
        pub(crate) struct NativeRepairPort {
            io: Arc<WindowsNativeIo>,
            lock: InstallerLock,
            selection: Arc<RepairSelection>,
            record: RepairRecord,
            permit: Option<Arc<RepairMutationPermit>>,
            deadline: Deadline,
            terminals: Vec<Arc<RepairTerminal>>,
            keeper: Option<Arc<Namespace>>,
            removal: Option<Arc<Namespace>>,
        }
        impl NativeRepairPort {
            pub(crate) fn new(
                io: Arc<WindowsNativeIo>,
                lock: InstallerLock,
                selection: Arc<RepairSelection>,
                record: RepairRecord,
                deadline: &Deadline,
            ) -> NativeResult<Self> {
                record.validate()?;
                let proof = io.admit_support(deadline)?;
                selection.reverify(&io, &proof, &lock, deadline)?;
                if record.plan() != selection.plan()
                    || record.cursor() != RepairCursor::Selected
                    || record.slot().is_some()
                {
                    return Err(NativeError::Foreign);
                }
                if !record.plan().archives().is_empty() {
                    let ctx = io.context.clone();
                    let lease = lock.0.clone();
                    let budget = proof.budget(&io, deadline)?;
                    let candidate = record.clone();
                    io.owner.run(Dispatch::Observation, deadline, move || {
                        validate_payload_lock(&ctx, &lease, &budget)?;
                        clean_publication(&lease.parent, &ctx, &budget)?;
                        let mut current = index(&lease.parent, &ctx, &budget)?;
                        verify_evidence(&lease.parent, &ctx, &current, &budget)?;
                        // Capacity test is pure on observed metadata; no reservation/write yet.
                        current
                            .reserve(candidate.operation(), candidate.plan().archives())
                            .map(|_| ())
                    })?;
                }
                let mut port = Self {
                    io,
                    lock,
                    selection,
                    record: record.clone(),
                    permit: None,
                    deadline: deadline.clone(),
                    terminals: Vec::new(),
                    keeper: None,
                    removal: None,
                };
                // All terminal facts are genuinely admitted BEFORE even selection/index writes.
                // Candidate phases in the read-only probe grant no publication/effect authority.
                for index in 0..record.plan().archives().len() {
                    port.terminal(&record, index as u8)?;
                }
                let proof = port.io.admit_support(deadline)?;
                let permit = publish_record(
                    &port.io,
                    &proof,
                    &port.lock,
                    &port.selection,
                    &record,
                    deadline,
                )?;
                port.permit = Some(permit);
                if !record.plan().archives().is_empty() {
                    let proof = port.io.admit_support(deadline)?;
                    let slot = reserve_index(
                        &port.io,
                        &proof,
                        &port.lock,
                        port.permit()?,
                        &port.selection,
                        deadline,
                    )?;
                    let mut bound = record;
                    bound.bind_slot(slot)?;
                    let proof = port.io.admit_support(deadline)?;
                    port.permit = Some(publish_record(
                        &port.io,
                        &proof,
                        &port.lock,
                        &port.selection,
                        &bound,
                        deadline,
                    )?);
                    port.record = bound;
                }
                Ok(port)
            }
            fn permit(&self) -> NativeResult<&Arc<RepairMutationPermit>> {
                self.permit.as_ref().ok_or(NativeError::Foreign)
            }
            pub(crate) fn run(&mut self, deadline: &Deadline) -> NativeResult<RepairOutcome> {
                self.deadline = deadline.clone();
                let mut record = self.record.clone();
                let result = repair::run_repair(self, &mut record);
                self.record = record;
                result
            }
            fn namespace(&mut self, source: &SourceSnapshot) -> NativeResult<Arc<Namespace>> {
                if let Some(namespace) = self.selection.archive_namespace() {
                    if source.kind() != SourceKind::OuterUpgrade {
                        return Err(NativeError::Foreign);
                    }
                    let proof = self.io.admit_support(&self.deadline)?;
                    namespace.reverify(&self.io, &proof, &self.deadline)?;
                    return Ok(Arc::new(Namespace::Keeper(namespace)));
                }
                let cell = if source.kind() == SourceKind::Removal {
                    &mut self.removal
                } else {
                    &mut self.keeper
                };
                if let Some(held) = cell {
                    let proof = self.io.admit_support(&self.deadline)?;
                    held.reverify(&self.io, &proof, source.source_operation(), &self.deadline)?;
                    return Ok(held.clone());
                }
                let proof = self.io.admit_support(&self.deadline)?;
                let io = self.io.clone();
                let context = io.context.clone();
                let lease = self.lock.0.clone();
                let budget = proof.budget(&io, &self.deadline)?;
                let deadline = self.deadline.clone();
                let operation = source.source_operation();
                let removal = source.kind() == SourceKind::Removal;
                let held=TERMINAL_OWNER.get_or_init(||Arc::new(CallOwner::default()))
                    .run(Dispatch::Observation,&self.deadline,move||{
                        // Original actual lock aliases remain in this worker through real native
                        // completion, even if the caller abandons the bounded observation.
                        validate_payload_lock(&context,&lease,&budget)?;
                        let value=if removal{
                            let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|_|NativeError::Unavailable)?;
                            let (server,lease)={let _entered=runtime.enter();
                                super::super::super::payload::helper::removal::RemovalKeeperLease::reserve(
                                    io.clone(),&proof,operation,&deadline)?};
                            Namespace::Removal(Arc::new(RemovalNamespace{lease,_server:server,_runtime:runtime}))
                        }else{Namespace::Keeper(Arc::new(keeper::reserve_repair_namespace(io.clone(),&proof,&deadline)?))};
                        budget.check()?;Ok(value)
                    })?;
                let held = Arc::new(held);
                *cell = Some(held.clone());
                Ok(held)
            }
        }
        impl NativeRepairPort {
            fn terminal(
                &mut self,
                record: &RepairRecord,
                index: u8,
            ) -> NativeResult<Arc<RepairTerminal>> {
                let proof = self.io.admit_support(&self.deadline)?;
                self.selection
                    .reverify(&self.io, &proof, &self.lock, &self.deadline)?;
                if record.plan() != self.selection.plan() {
                    return Err(NativeError::Foreign);
                }
                let source = record
                    .plan()
                    .archives()
                    .get(usize::from(index))
                    .ok_or(NativeError::Invalid)?
                    .clone();
                if let Some(held) = self.terminals.iter().find(|t| t.snapshot == source) {
                    let proof = self.io.admit_support(&self.deadline)?;
                    held.reverify(&self.io, &proof, &self.lock, record, &self.deadline)?;
                    return Ok(held.clone());
                }
                let namespace = self.namespace(&source)?;
                let ctx = self.io.context.clone();
                let lease = self.lock.0.clone();
                let budget = proof.budget(&self.io, &self.deadline)?;
                let selected = record.clone();
                let observed = source.clone();
                let held_namespace = namespace.clone();
                let (bytes, document, associated) =
                    self.io
                        .owner
                        .run(Dispatch::Observation, &self.deadline, move || {
                            validate_payload_lock(&ctx, &lease, &budget)?;
                            held_namespace.bound(&ctx, &budget)?;
                            let (state, bytes) =
                                source_at(&lease.parent, &ctx, &selected, &observed, &budget)?;
                            if !matches!(
                                state,
                                ArchiveObservation::SourceOnly
                                    | ArchiveObservation::DestinationOnly
                            ) {
                                return Err(NativeError::Foreign);
                            }
                            let bytes = bytes.ok_or(NativeError::Foreign)?;
                            let document = terminal_document(
                                &lease.parent,
                                &ctx,
                                observed.kind(),
                                &bytes,
                                &budget,
                            )?;
                            if source_operation(&document) != observed.source_operation() {
                                return Err(NativeError::Foreign);
                            }
                            let associated = associated(&lease.parent, &ctx, &observed, &budget)?;
                            Ok((bytes, document, associated))
                        })?;
                let file = if matches!(document, TerminalDocument::File(_)) {
                    // Existing genuine ended-logon FILE seal/convergence admission, no cleanup.
                    // A cold already-archived FileRecovery source cannot reconstruct that seal.
                    let proof = self.io.admit_support(&self.deadline)?;
                    let seal = self
                        .io
                        .prepare_file_recovery(&proof, &self.lock, &self.deadline)?
                        .ok_or(NativeError::Unsupported)?;
                    let proof = self.io.admit_support(&self.deadline)?;
                    let permit = self.io.admit_file_recovery_permit(
                        &proof,
                        &self.lock,
                        &seal,
                        &self.deadline,
                    )?;
                    let proof = self.io.admit_support(&self.deadline)?;
                    let root =
                        self.io
                            .file_recovery_root(&proof, &self.lock, &seal, &self.deadline)?;
                    let proof = self.io.admit_support(&self.deadline)?;
                    root.observe_terminal(&proof, &self.lock, &seal, &permit, &self.deadline)?;
                    Some(FileAuthority {
                        _seal: seal,
                        _root: root,
                    })
                } else {
                    None
                };
                let terminal = Arc::new(RepairTerminal {
                    io: self.io.clone(),
                    snapshot: source,
                    bytes,
                    namespace,
                    associated,
                    _file: file,
                });
                let proof = self.io.admit_support(&self.deadline)?;
                terminal.reverify(&self.io, &proof, &self.lock, record, &self.deadline)?;
                self.terminals.push(terminal.clone());
                Ok(terminal)
            }
        }
        impl RepairPort for NativeRepairPort {
            type Terminal = Arc<RepairTerminal>;
            fn renew(&mut self, record: &RepairRecord) -> NativeResult<()> {
                if record != self.permit()?.record() {
                    return Err(NativeError::Foreign);
                }
                let proof = self.io.admit_support(&self.deadline)?;
                self.permit()?.reverify(
                    &self.io,
                    &proof,
                    &self.lock,
                    &self.selection,
                    &self.deadline,
                )
            }
            fn persist(&mut self, record: &RepairRecord) -> NativeResult<()> {
                let proof = self.io.admit_support(&self.deadline)?;
                self.permit = Some(publish_record(
                    &self.io,
                    &proof,
                    &self.lock,
                    &self.selection,
                    record,
                    &self.deadline,
                )?);
                Ok(())
            }
            fn task_observe(
                &mut self,
                _: &RepairRecord,
            ) -> NativeResult<repair::RepairTaskObservation> {
                let proof = self.io.admit_support(&self.deadline)?;
                self.io
                    .observe_repair_task(&proof, &self.lock, &self.selection, &self.deadline)
            }
            fn register_task(&mut self, record: &RepairRecord) -> NativeResult<()> {
                self.renew(record)?;
                if record.cursor() != RepairCursor::TaskIntent {
                    return Err(NativeError::Foreign);
                }
                let proof = self.io.admit_support(&self.deadline)?;
                self.io.register_repair_task(
                    &proof,
                    &self.lock,
                    &self.selection,
                    self.permit()?,
                    &self.deadline,
                )
            }
            fn admit_terminal(
                &mut self,
                record: &RepairRecord,
                index: u8,
            ) -> NativeResult<Self::Terminal> {
                self.renew(record)?;
                self.terminal(record, index)
            }
            fn observe_archive(
                &mut self,
                record: &RepairRecord,
                index: u8,
            ) -> NativeResult<ArchiveObservation> {
                self.renew(record)?;
                if record.cursor() != (RepairCursor::ArchiveIntent { index }) {
                    return Err(NativeError::Foreign);
                }
                let proof = self.io.admit_support(&self.deadline)?;
                let ctx = self.io.context.clone();
                let lease = self.lock.0.clone();
                let record = record.clone();
                let budget = proof.budget(&self.io, &self.deadline)?;
                let source = record
                    .plan()
                    .archives()
                    .get(usize::from(index))
                    .ok_or(NativeError::Invalid)?
                    .clone();
                self.io
                    .owner
                    .run(Dispatch::Observation, &self.deadline, move || {
                        validate_payload_lock(&ctx, &lease, &budget)?;
                        Ok(source_at(&lease.parent, &ctx, &record, &source, &budget)?.0)
                    })
            }
            fn archive_exact(
                &mut self,
                record: &RepairRecord,
                index: u8,
                terminal: &Self::Terminal,
            ) -> NativeResult<()> {
                self.renew(record)?;
                if record.cursor() != (RepairCursor::ArchiveIntent { index }) {
                    return Err(NativeError::Foreign);
                }
                let source = record
                    .plan()
                    .archives()
                    .get(usize::from(index))
                    .ok_or(NativeError::Invalid)?;
                if terminal.snapshot != *source {
                    return Err(NativeError::Foreign);
                }
                let proof = self.io.admit_support(&self.deadline)?;
                terminal.reverify(&self.io, &proof, &self.lock, record, &self.deadline)?;
                let ctx = self.io.context.clone();
                let lease = self.lock.0.clone();
                let budget = proof.budget(&self.io, &self.deadline)?;
                let record = record.clone();
                let terminal = terminal.clone();
                let expected = self.permit()?.bytes.clone();
                let owner = self.io.owner.clone();
                self.io
                    .owner
                    .run(Dispatch::Mutation, &self.deadline, move || {
                        let change = Change::new();
                        let result = change.finish((|| {
                            validate_payload_lock(&ctx, &lease, &budget)?;
                            terminal.namespace.bound(&ctx, &budget)?;
                            let (_, actual) =
                                read(&lease.parent, &ctx, records::RecordName::Repair, &budget)?
                                    .ok_or(NativeError::Foreign)?;
                            if actual != expected {
                                return Err(NativeError::Foreign);
                            }
                            check_slot(&lease.parent, &ctx, &record, &budget)?;
                            if source_at(&lease.parent, &ctx, &record, &terminal.snapshot, &budget)?
                                .0
                                != ArchiveObservation::SourceOnly
                            {
                                return Err(NativeError::OutcomeUnknown);
                            }
                            terminal_document(
                                &lease.parent,
                                &ctx,
                                terminal.snapshot.kind(),
                                &terminal.bytes,
                                &budget,
                            )?;
                            for (name, expected) in &terminal.associated {
                                if observed(&read(&lease.parent, &ctx, name.clone(), &budget)?)?
                                    != *expected
                                {
                                    return Err(NativeError::Foreign);
                                }
                            }
                            change.reached();
                            let destination = lease
                                .parent
                                .repair_evidence_slot(
                                    record.slot().ok_or(NativeError::Foreign)?,
                                    true,
                                    &ctx.security,
                                    &budget,
                                )?
                                .ok_or(NativeError::OutcomeUnknown)?;
                            lease.parent.archive_repair_metadata(
                                terminal.snapshot.kind().leaf(),
                                identity(terminal.snapshot.stamp()),
                                &terminal.bytes,
                                &destination,
                                &ctx.security,
                                &budget,
                            )?;
                            terminal.namespace.bound(&ctx, &budget)
                        })());
                        if matches!(result, Err(NativeError::OutcomeUnknown)) {
                            owner.retire_mutations();
                        }
                        result
                    })
            }
            fn finish(&mut self, record: &RepairRecord) -> NativeResult<()> {
                self.renew(record)?;
                if record.cursor() != RepairCursor::Complete {
                    return Err(NativeError::Foreign);
                }
                for index in 0..record.plan().archives().len() {
                    let terminal = self.admit_terminal(record, index as u8)?;
                    let proof = self.io.admit_support(&self.deadline)?;
                    terminal.reverify(&self.io, &proof, &self.lock, record, &self.deadline)?;
                }
                if record.plan().archives().is_empty() {
                    return Ok(());
                }
                let proof = self.io.admit_support(&self.deadline)?;
                let ctx = self.io.context.clone();
                let lease = self.lock.0.clone();
                let budget = proof.budget(&self.io, &self.deadline)?;
                let record = record.clone();
                let expected = self.permit()?.bytes.clone();
                let owner = self.io.owner.clone();
                self.io
                    .owner
                    .run(Dispatch::Mutation, &self.deadline, move || {
                        let change = Change::new();
                        let result = change.finish((|| {
                            validate_payload_lock(&ctx, &lease, &budget)?;
                            clean_publication(&lease.parent, &ctx, &budget)?;
                            let (_, actual) =
                                read(&lease.parent, &ctx, records::RecordName::Repair, &budget)?
                                    .ok_or(NativeError::Foreign)?;
                            if actual != expected {
                                return Err(NativeError::Foreign);
                            }
                            check_slot(&lease.parent, &ctx, &record, &budget)?;
                            for source in record.plan().archives() {
                                if source_at(&lease.parent, &ctx, &record, source, &budget)?.0
                                    != ArchiveObservation::DestinationOnly
                                {
                                    return Err(NativeError::OutcomeUnknown);
                                }
                            }
                            let mut index = index(&lease.parent, &ctx, &budget)?;
                            index.complete(
                                record.slot().ok_or(NativeError::Foreign)?,
                                record.operation(),
                            )?;
                            publish_fixed(
                                &ctx,
                                &lease,
                                record.operation(),
                                PublicationTarget::EvidenceIndex,
                                &index.encode()?,
                                &budget,
                                &change,
                            )
                        })());
                        if matches!(result, Err(NativeError::OutcomeUnknown)) {
                            owner.retire_mutations();
                        }
                        result
                    })
            }
        }
        impl RepairProbe {
            pub(crate) fn apply(
                &self,
                plan: &repair::RepairPlan,
                deadline: &Deadline,
            ) -> NativeResult<RepairOutcome> {
                let io = self.io().ok_or(NativeError::Unsupported)?.clone();
                let proof = io.admit_support(deadline)?;
                let lock = io.acquire_installer_lock(&proof, deadline)?;
                let proof = io.admit_support(deadline)?;
                let selection =
                    io.select_repair(&proof, &lock, self.observation(), plan, deadline)?;
                let record = RepairRecord::new(
                    nonce()?,
                    recovery::OuterContextCorrelation::new(io.target().identity())?,
                    plan.clone(),
                    None,
                )?;
                NativeRepairPort::new(io, lock, selection, record, deadline)?.run(deadline)
            }
        }
        impl WindowsNativeIo {
            /// Read-only correlation for integration's capacity deferral. The old a6
            /// classifier stays unchanged; these facts authorize no native effect.
            pub(crate) fn repair_capacity_only(
                &self,
                proof: &SupportProof,
                observed: &[JournalObservation],
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                let ctx = self.context.clone();
                let budget = proof.budget(self, deadline)?;
                let observed = observed.to_vec();
                self.owner.run(Dispatch::Observation, deadline, move || {
                    ctx.validate(&budget)?;
                    let parent =
                        Anchor::open(ctx.target.paths.installer(), &ctx.security, true, &budget)?
                            .ok_or(NativeError::Missing)?;
                    clean_publication(&parent, &ctx, &budget)?;
                    let evidence = index(&parent, &ctx, &budget)?;
                    verify_evidence(&parent, &ctx, &evidence, &budget)?;
                    let (_, bytes) = read(&parent, &ctx, records::RecordName::Repair, &budget)?
                        .ok_or(NativeError::Foreign)?;
                    let record = RepairRecord::decode(&bytes)?;
                    record.context().same_user(&ctx.target.identity)?;
                    if record.cursor() != RepairCursor::Complete {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    if record.slot().is_some() {
                        check_slot(&parent, &ctx, &record, &budget)?;
                    }
                    if (0..3).any(|slot| evidence.get(slot).is_none()) {
                        return Ok(false);
                    }
                    let mut terminal = false;
                    for kind in [
                        SourceKind::OuterUpgrade,
                        SourceKind::FileRecovery,
                        SourceKind::Removal,
                    ] {
                        let current = match read(&parent, &ctx, source_name(kind), &budget)? {
                            None => JournalObservation::Absent(kind),
                            Some((id, bytes)) => {
                                let document =
                                    terminal_document(&parent, &ctx, kind, &bytes, &budget)?;
                                terminal = true;
                                JournalObservation::Terminal(snapshot(
                                    kind,
                                    source_operation(&document),
                                    id,
                                    &bytes,
                                )?)
                            }
                        };
                        if observed.iter().find(|j| j.kind() == kind) != Some(&current) {
                            return Err(NativeError::Foreign);
                        }
                    }
                    budget.check()?;
                    Ok(terminal)
                })
            }
            /// Explicit settlement only: consume the real lock and retain the original
            /// vacant namespace. No payload health, image pin or task authority is minted.
            pub(crate) fn archive_settled_outer_history(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: InstallerLock,
                absence: &keeper::KeeperCopyAbsent,
                expected: Vec<u8>,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                self.lock_binding(proof, &lock, deadline)?;
                self.refuse_unsettled_repair(proof, &lock, deadline)?;
                self.refuse_unsettled_payload_repair(proof, &lock, deadline)?;
                let namespace = absence.archive_namespace(self, proof, &lock, deadline)?;
                let ctx = self.context.clone();
                let lease = lock.0.clone();
                let budget = proof.budget(self, deadline)?;
                let archive_operation = nonce()?;
                let source_operation_id = absence.operation();
                let (source, capacity) =
                    self.owner.run(Dispatch::Observation, deadline, move || {
                        validate_payload_lock(&ctx, &lease, &budget)?;
                        clean_publication(&lease.parent, &ctx, &budget)?;
                        let mut evidence = index(&lease.parent, &ctx, &budget)?;
                        verify_evidence(&lease.parent, &ctx, &evidence, &budget)?;
                        let (id, bytes) = read(
                            &lease.parent,
                            &ctx,
                            records::RecordName::OuterUpgrade,
                            &budget,
                        )?
                        .ok_or(NativeError::Foreign)?;
                        if bytes != expected {
                            return Err(NativeError::Foreign);
                        }
                        let document = terminal_document(
                            &lease.parent,
                            &ctx,
                            SourceKind::OuterUpgrade,
                            &bytes,
                            &budget,
                        )?;
                        if source_operation(&document) != source_operation_id {
                            return Err(NativeError::Foreign);
                        }
                        // An unrelated active or uncertain selection cannot be bypassed even
                        // when capacity is exhausted. Terminal evidence is never evicted.
                        for kind in [SourceKind::FileRecovery, SourceKind::Removal] {
                            if let Some((_, bytes)) =
                                read(&lease.parent, &ctx, source_name(kind), &budget)?
                            {
                                terminal_document(&lease.parent, &ctx, kind, &bytes, &budget)?;
                            }
                        }
                        let source =
                            snapshot(SourceKind::OuterUpgrade, source_operation_id, id, &bytes)?;
                        let capacity = match evidence
                            .reserve(archive_operation, std::slice::from_ref(&source))
                        {
                            Ok(_) => true,
                            // Only this pure, positively observed capacity refusal is deferred.
                            // No selection, intent, index or archive write has occurred.
                            Err(NativeError::Busy) => false,
                            Err(error) => return Err(error),
                        };
                        budget.check()?;
                        Ok((source, capacity))
                    })?;
                if !capacity {
                    return Ok(false);
                }
                let plan = repair::RepairPlan::outer_history(source)?;
                let selection =
                    RepairSelection::outer_history(self.clone(), plan.clone(), namespace)?;
                let record = RepairRecord::new(
                    archive_operation,
                    recovery::OuterContextCorrelation::new(self.target().identity())?,
                    plan,
                    None,
                )?;
                let outcome =
                    NativeRepairPort::new(self.clone(), lock, selection, record, deadline)?
                        .run(deadline)?;
                if outcome != RepairOutcome::Repaired {
                    return Err(NativeError::OutcomeUnknown);
                }
                Ok(true)
            }
        }
    }
    // A6 diagnostic/task-only region. No old constructor, Stop, keeper or payload mutation.
    #[cfg(not(test))]
    mod repair_probe {
        use super::super::super::{
            payload::inventory::ApprovedInventory,
            repair::{
                RepairDiagnostic as Diagnostic, RepairObservation, RepairPlan,
                RepairTaskObservation,
            },
            service::task::{Definition, Logon, RunLevel, SUPERVISOR_ARGUMENT, TASK_NAME},
            transport::{self, Endpoint, security::NativeEndpoint},
        };
        use super::*;
        use crate::agent_contract::{
            DecodedReply, InstallerRequest, ObservationSource, StatusAdmission,
        };
        use native::{RepairRead, repair_readonly_diagnostic};
        use std::sync::OnceLock;
        static STATUS: OnceLock<Arc<CallOwner>> = OnceLock::new();

        pub(crate) struct RepairProbe {
            io: Option<Arc<WindowsNativeIo>>,
            observation: RepairObservation,
        }
        impl RepairProbe {
            pub(crate) fn observation(&self) -> &RepairObservation {
                &self.observation
            }
            pub(crate) fn io(&self) -> Option<&Arc<WindowsNativeIo>> {
                self.io.as_ref()
            }
        }
        fn diagnostic(error: NativeError) -> Diagnostic {
            match error {
                NativeError::Missing => Diagnostic::Missing,
                NativeError::Foreign => Diagnostic::UnsafeForeign,
                NativeError::Unavailable | NativeError::Busy => Diagnostic::Unavailable,
                _ => Diagnostic::Unknown,
            }
        }
        fn read_diagnostic<T>(read: &RepairRead<T>) -> Diagnostic {
            match read {
                RepairRead::Present(_) => Diagnostic::Healthy,
                RepairRead::Missing => Diagnostic::Missing,
                RepairRead::AccessDenied => Diagnostic::AccessDenied,
                RepairRead::Unsafe => Diagnostic::UnsafeForeign,
                RepairRead::Unavailable => Diagnostic::Unavailable,
                RepairRead::Unknown => Diagnostic::Unknown,
            }
        }
        fn failed(diagnostic: Diagnostic) -> NativeResult<RepairObservation> {
            RepairObservation::new(
                diagnostic,
                RepairTaskObservation::new(Diagnostic::Unavailable, None)?,
                Diagnostic::Unavailable,
                Vec::new(),
                Diagnostic::Unknown,
            )
        }
        fn desired(io: &WindowsNativeIo, install: &str) -> Definition {
            let user = io.context.target.identity.user.sddl();
            Definition {
                name: TASK_NAME.into(),
                principal: user.clone(),
                trigger_user: user,
                logon: Logon::InteractiveToken,
                run_level: RunLevel::Limited,
                action: format!("{install}\\crosspane-installer.exe"),
                arguments: SUPERVISOR_ARGUMENT.into(),
                working_directory: install.into(),
                logon_trigger_only: true,
                ignore_new_instance: true,
                manager_restart_count: 0,
                enabled: true,
            }
        }
        struct RepairImages {
            module: SelfImagePin,
            images: [OpenedPe; 4],
            definition: Definition,
        }
        impl RepairImages {
            fn check(&self, io: &WindowsNativeIo, deadline: &Deadline) -> NativeResult<()> {
                let proof = io.admit_support(deadline)?;
                self.module.reverify(io, &proof, deadline)?;
                for image in &self.images {
                    let proof = io.admit_support(deadline)?;
                    image.reverify(io, &proof, deadline)?;
                }
                deadline.check()
            }
        }
        fn images(
            io: &Arc<WindowsNativeIo>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<(Diagnostic, Option<Arc<RepairImages>>, Definition)> {
            let fallback = desired(io, io.context.target.paths.install());
            let inventory = match ApprovedInventory::embedded() {
                Ok(inventory) => inventory,
                // No manifest/current-byte/catalog fallback. Approval availability is separate.
                Err(_) => return Ok((Diagnostic::Unknown, None, fallback)),
            };
            let module = match io.repair_own_image(proof, deadline)? {
                RepairRead::Present(module) => module,
                other => return Ok((read_diagnostic(&other), None, fallback)),
            };
            let installer = ApprovedPe::own_image(&module)?;
            let pins = [
                installer,
                inventory.role(PayloadRole::Agent)?.clone(),
                inventory.role(PayloadRole::Ui)?.clone(),
                inventory.role(PayloadRole::Ctl)?.clone(),
            ];
            let budget = proof.budget(io, deadline)?;
            let context = io.context.clone();
            let value = io.owner.run(Dispatch::Observation, deadline, move || {
                let mut definition = fallback;
                let root = repair_readonly_diagnostic(|| {
                    context.validate(&budget)?;
                    Anchor::open(
                        context.target.paths.install(),
                        &context.security,
                        true,
                        &budget,
                    )
                    .map(|root| root.map(Arc::new))
                });
                let root = match root {
                    RepairRead::Present(root) => root,
                    other => return Ok((read_diagnostic(&other), None, definition)),
                };
                definition.action = format!(
                    "{}\\crosspane-installer.exe",
                    root.canonical_dos_path(&context.security, &budget)?
                        .to_str()
                        .ok_or(NativeError::Unsupported)?
                );
                definition.working_directory = root
                    .canonical_dos_path(&context.security, &budget)?
                    .to_str()
                    .ok_or(NativeError::Unsupported)?
                    .to_owned();
                let mut admitted = Vec::with_capacity(4);
                let mut damage = Diagnostic::Healthy;
                for (role, expected) in PayloadRole::ALL.into_iter().zip(pins) {
                    let malformed = std::cell::Cell::new(false);
                    let read = repair_readonly_diagnostic(|| {
                        context.validate(&budget)?;
                        let image = match root.open_image(
                            role.leaf(),
                            true,
                            expected.version(),
                            &context.security,
                            &budget,
                        ) {
                            Ok(image) => image,
                            Err(error) => {
                                malformed.set(matches!(
                                    error,
                                    NativeError::Invalid | NativeError::Unsupported
                                ));
                                return Err(error);
                            }
                        };
                        Ok(Some(image))
                    });
                    match read {
                        RepairRead::Present(image) if image.facts == *expected.facts() => {
                            admitted.push(OpenedPe(Arc::new(ApprovedImage {
                                target: context.target.nonce,
                                parent: root.clone(),
                                leaf: role.leaf().into(),
                                image,
                                expected,
                            })));
                        }
                        RepairRead::Present(_) => {
                            damage = combine(damage, Diagnostic::Mismatch);
                        }
                        other => {
                            let fact = if malformed.get() && matches!(other, RepairRead::Unknown) {
                                Diagnostic::Mismatch
                            } else {
                                read_diagnostic(&other)
                            };
                            damage = combine(damage, fact);
                        }
                    }
                }
                budget.check()?;
                if damage != Diagnostic::Healthy {
                    return Ok((damage, None, definition));
                }
                let images: [OpenedPe; 4] =
                    admitted.try_into().map_err(|_| NativeError::Invalid)?;
                Ok((damage, Some(images), definition))
            });
            match value {
                Ok((diagnostic, images, definition)) => {
                    let images = images.map(|images| {
                        Arc::new(RepairImages {
                            module,
                            images,
                            definition: definition.clone(),
                        })
                    });
                    Ok((diagnostic, images, definition))
                }
                Err(error) => Ok((
                    diagnostic(error),
                    None,
                    desired(io, io.context.target.paths.install()),
                )),
            }
        }
        fn combine(current: Diagnostic, new: Diagnostic) -> Diagnostic {
            // Preserve proven denial/unsafe/mismatch rather than replacing it with a later missing
            // leaf. This aggregate remains diagnostic only; every role must be Healthy to apply.
            fn rank(value: Diagnostic) -> u8 {
                match value {
                    Diagnostic::Healthy => 0,
                    Diagnostic::Missing => 1,
                    Diagnostic::Unknown => 2,
                    Diagnostic::Unavailable => 3,
                    Diagnostic::Mismatch => 4,
                    Diagnostic::UnsafeForeign => 5,
                    Diagnostic::AccessDenied => 6,
                    Diagnostic::Disabled => 7,
                }
            }
            if rank(new) > rank(current) {
                new
            } else {
                current
            }
        }
        struct RepairAgent {
            observation: AgentObservation,
        }
        impl RepairAgent {
            fn check(
                &self,
                io: &Arc<WindowsNativeIo>,
                images: &RepairImages,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                let proof = io.admit_support(deadline)?;
                self.observation.revalidate(io, &proof, deadline)?;
                if self.observation.bootstrap().phase
                    != crate::agent_contract::BootstrapPhase::Ready
                {
                    return Err(NativeError::Foreign);
                }
                if io.agent_identity(&self.observation, &proof, deadline)?
                    != images.images[1].identity()
                {
                    return Err(NativeError::Foreign);
                }
                // The original object/pins stay retained. Each new endpoint gets fresh support,
                // and cannot switch to a new instance merely because the old proof aged out.
                let proof = io.admit_support(deadline)?;
                let endpoint = Arc::new(NativeEndpoint::admit(io.clone(), proof, deadline)?);
                if endpoint.selected_instance()? != self.observation.bootstrap().instance_id
                    || status(endpoint, deadline)? != Diagnostic::Healthy
                {
                    return Err(NativeError::Foreign);
                }
                deadline.check()
            }
        }
        fn status(endpoint: Arc<NativeEndpoint>, deadline: &Deadline) -> NativeResult<Diagnostic> {
            let budget = deadline.clone();
            STATUS.get_or_init(|| Arc::new(CallOwner::default())).run(
                Dispatch::Observation,
                deadline,
                move || {
                    let read = repair_readonly_diagnostic(|| {
                        let (reply, source) =
                            transport::run(endpoint.as_ref(), &InstallerRequest::Status, &budget);
                        let reply = reply.map_err(|_| NativeError::Unavailable)?;
                        if source != ObservationSource::Live {
                            return Ok(Some(Diagnostic::Unknown));
                        }
                        match reply {
                            DecodedReply::Status(StatusAdmission::Supported(health))
                                if health.installer().build.version
                                    == env!("CARGO_PKG_VERSION") =>
                            {
                                Ok(Some(Diagnostic::Healthy))
                            }
                            _ => Ok(Some(Diagnostic::Unknown)),
                        }
                    });
                    Ok(match read {
                        RepairRead::Present(value) => value,
                        other => read_diagnostic(&other),
                    })
                },
            )
        }
        fn agent(
            io: &Arc<WindowsNativeIo>,
            proof: &SupportProof,
            images: Option<&RepairImages>,
            deadline: &Deadline,
        ) -> NativeResult<(Diagnostic, Option<Arc<RepairAgent>>)> {
            let read = io.repair_agent_observation(proof, deadline)?;
            let observation = match read {
                RepairRead::Present(value) => value,
                other => return Ok((read_diagnostic(&other), None)),
            };
            if observation.bootstrap().phase != crate::agent_contract::BootstrapPhase::Ready {
                return Ok((Diagnostic::Unknown, None));
            }
            let proof = io.admit_support(deadline)?;
            let endpoint = match NativeEndpoint::admit(io.clone(), proof, deadline) {
                Ok(endpoint) => Arc::new(endpoint),
                Err(error) => return Ok((diagnostic(error), None)),
            };
            // Both factories observe the actual same original fixed instance. A concurrent restart
            // is Unknown, never a healthy registration capability.
            if endpoint.selected_instance()? != observation.bootstrap().instance_id {
                return Ok((Diagnostic::Unknown, None));
            }
            let actual = status(endpoint.clone(), deadline)?;
            if actual != Diagnostic::Healthy {
                return Ok((actual, None));
            }
            let Some(images) = images else {
                return Ok((Diagnostic::Unknown, None));
            };
            let proof = io.admit_support(deadline)?;
            if io.agent_identity(&observation, &proof, deadline)? != images.images[1].identity() {
                return Ok((Diagnostic::Mismatch, None));
            }
            Ok((
                Diagnostic::Healthy,
                Some(Arc::new(RepairAgent { observation })),
            ))
        }
        struct Detection {
            observation: RepairObservation,
            images: Option<Arc<RepairImages>>,
            agent: Option<Arc<RepairAgent>>,
        }
        fn detect(
            io: &Arc<WindowsNativeIo>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Detection> {
            let (payload, images, definition) = images(io, proof, deadline)?;
            let task =
                super::super::super::service::task::repair_probe(io.clone(), definition, deadline)?;
            let proof = io.admit_support(deadline)?;
            let (agent, agent_pin) = match agent(io, &proof, images.as_deref(), deadline) {
                Ok(value) => value,
                Err(error) => (diagnostic(error), None),
            };
            let proof = io.admit_support(deadline)?;
            let debt = io.inspect_repair_debt(&proof, deadline)?;
            let (journals, publication) = debt.into_parts();
            let observation = RepairObservation::new(payload, task, agent, journals, publication)?;
            Ok(Detection {
                observation,
                images,
                agent: agent_pin,
            })
        }
        pub(crate) struct RepairSelection {
            io: Arc<WindowsNativeIo>,
            plan: RepairPlan,
            authority: RepairAuthority,
        }
        enum RepairAuthority {
            Full {
                images: Arc<RepairImages>,
                agent: Arc<RepairAgent>,
            },
            OuterHistory(Arc<keeper::RepairNamespaceHold>),
        }
        impl RepairSelection {
            // Sole caller is the lock-owning settlement bridge after genuine absence
            // admission and exact terminal-byte selection. Never deserialized.
            pub(super) fn outer_history(
                io: Arc<WindowsNativeIo>,
                plan: RepairPlan,
                namespace: Arc<keeper::RepairNamespaceHold>,
            ) -> NativeResult<Arc<Self>> {
                plan.validate()?;
                if plan.task_xml().is_some()
                    || plan.archives().len() != 1
                    || plan.archives()[0].kind()
                        != super::super::super::repair::SourceKind::OuterUpgrade
                {
                    return Err(NativeError::Foreign);
                }
                Ok(Arc::new(Self {
                    io,
                    plan,
                    authority: RepairAuthority::OuterHistory(namespace),
                }))
            }
            fn full_images(&self) -> NativeResult<&Arc<RepairImages>> {
                match &self.authority {
                    RepairAuthority::Full { images, .. } => Ok(images),
                    RepairAuthority::OuterHistory(_) => Err(NativeError::Foreign),
                }
            }
            pub(super) fn archive_namespace(&self) -> Option<Arc<keeper::RepairNamespaceHold>> {
                match &self.authority {
                    RepairAuthority::OuterHistory(namespace) => Some(namespace.clone()),
                    RepairAuthority::Full { .. } => None,
                }
            }
            pub(crate) fn plan(&self) -> &RepairPlan {
                &self.plan
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
                let context = io.context.clone();
                let lease = lock.0.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Observation, deadline, move || {
                    validate_payload_lock(&context, &lease, &budget)
                })?;
                match &self.authority {
                    RepairAuthority::Full { images, agent } => {
                        images.check(io, deadline)?;
                        agent.check(&self.io, images, deadline)
                    }
                    // Source renewals now belong to the unchanged archive permit and
                    // terminal driver, including after the source has moved. Retain the
                    // original kernel namespace rather than reacquiring it by name.
                    RepairAuthority::OuterHistory(namespace) => {
                        namespace.reverify(io, proof, deadline)
                    }
                }
            }
        }
        /// Retains the actual current lock/pins/record before the separate MTA mutation dispatch.
        /// This is not Clone/Deserialize, nor a capability constructible from diagnostic facts.
        pub(crate) struct RepairTaskBinding {
            selection: Arc<RepairSelection>,
            images: Arc<RepairImages>,
            lease: Arc<LockState>,
            permit: Arc<super::repair_archive::RepairMutationPermit>,
            change: Change,
        }
        impl RepairTaskBinding {
            pub(crate) fn check(&self, deadline: &Deadline) -> NativeResult<()> {
                let io = &self.selection.io;
                self.selection.full_images()?;
                let proof = io.admit_support(deadline)?;
                let lock = InstallerLock(self.lease.clone());
                self.selection.reverify(io, &proof, &lock, deadline)?;
                let proof = io.admit_support(deadline)?;
                self.permit
                    .reverify(io, &proof, &lock, &self.selection, deadline)?;
                if self.permit.record().cursor()
                    != super::super::super::repair::record::RepairCursor::TaskIntent
                {
                    return Err(NativeError::Foreign);
                }
                deadline.check()
            }
            pub(crate) fn desired(&self) -> &Definition {
                &self.images.definition
            }
            pub(crate) fn task_xml(&self) -> NativeResult<&str> {
                self.selection.plan.task_xml().ok_or(NativeError::Foreign)
            }
            pub(crate) fn reached(&self) {
                self.change.reached();
            }
            pub(crate) fn finish<T>(&self, result: NativeResult<T>) -> NativeResult<T> {
                self.change.finish(result)
            }
            pub(crate) fn retire(&self) {
                self.selection.io.owner.retire_mutations();
            }
        }
        impl WindowsNativeIo {
            pub(crate) fn observe_payload_repair(
                self: &Arc<Self>,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<RepairObservation> {
                proof.check(self, deadline)?;
                detect(self, proof, deadline).map(|actual| actual.observation)
            }
            pub(crate) fn probe_repair(
                clock: Arc<dyn Clock>,
                deadline: &Deadline,
            ) -> NativeResult<RepairProbe> {
                identity::native::refuse_impersonation()?;
                let owner = Arc::new(CallOwner::default());
                let budget = deadline.clone();
                let read = owner.run(Dispatch::Observation, deadline, move || {
                    Ok(repair_readonly_diagnostic(|| {
                        Context::current(&budget).map(Some)
                    }))
                })?;
                let context = match read {
                    RepairRead::Present(context) => context,
                    other => {
                        return Ok(RepairProbe {
                            io: None,
                            observation: failed(read_diagnostic(&other))?,
                        });
                    }
                };
                let io = Arc::new(Self {
                    context: Arc::new(context),
                    owner,
                    clock,
                });
                let proof = io.admit_support(deadline)?;
                let observed = detect(&io, &proof, deadline)?;
                Ok(RepairProbe {
                    io: Some(io),
                    observation: observed.observation,
                })
            }
            pub(crate) fn select_repair(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                original: &RepairObservation,
                plan: &RepairPlan,
                deadline: &Deadline,
            ) -> NativeResult<Arc<RepairSelection>> {
                self.lock_binding(proof, lock, deadline)?;
                self.refuse_unsettled_payload_repair(proof, lock, deadline)?;
                // Supplied model facts grant nothing: freshly observe and independently classify.
                let actual = detect(self, proof, deadline)?;
                if &actual.observation != original {
                    return Err(NativeError::Foreign);
                }
                match super::super::super::repair::classify(&actual.observation)? {
                    super::super::super::repair::RepairDecision::Apply(actual_plan)
                        if &actual_plan == plan => {}
                    _ => return Err(NativeError::Foreign),
                }
                let selected = Arc::new(RepairSelection {
                    io: self.clone(),
                    plan: plan.clone(),
                    authority: RepairAuthority::Full {
                        images: actual.images.ok_or(NativeError::Foreign)?,
                        agent: actual.agent.ok_or(NativeError::Foreign)?,
                    },
                });
                let proof = self.admit_support(deadline)?;
                selected.reverify(self, &proof, lock, deadline)?;
                Ok(selected)
            }
            pub(crate) fn observe_repair_task(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                selection: &Arc<RepairSelection>,
                deadline: &Deadline,
            ) -> NativeResult<RepairTaskObservation> {
                selection.reverify(self, proof, lock, deadline)?;
                super::super::super::service::task::repair_probe(
                    selection.io.clone(),
                    selection.full_images()?.definition.clone(),
                    deadline,
                )
            }
            pub(crate) fn register_repair_task(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                selection: &Arc<RepairSelection>,
                permit: &Arc<super::repair_archive::RepairMutationPermit>,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                selection.reverify(self, proof, lock, deadline)?;
                permit.reverify(self, proof, lock, selection, deadline)?;
                let binding = Arc::new(RepairTaskBinding {
                    selection: selection.clone(),
                    images: selection.full_images()?.clone(),
                    lease: lock.0.clone(),
                    permit: permit.clone(),
                    change: Change::new(),
                });
                super::super::super::service::task::repair_register(binding, deadline)
            }
            fn repair_own_image(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<RepairRead<SelfImagePin>> {
                use windows_sys::Win32::System::Threading::{
                    GetCurrentProcess, QueryFullProcessImageNameW,
                };
                let budget = proof.budget(self, deadline)?;
                let context = self.context.clone();
                self.owner.run(Dispatch::Observation, deadline, move || {
                    Ok(repair_readonly_diagnostic(|| {
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
                        let (parent, leaf) =
                            path.rsplit_once('\\').ok_or(NativeError::Unsupported)?;
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
                        Ok(Some(SelfImagePin(Arc::new(OwnImage {
                            target: context.target.nonce,
                            parent,
                            leaf: leaf.into(),
                            image,
                        }))))
                    }))
                })
            }
            fn repair_agent_observation(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<RepairRead<AgentObservation>> {
                let budget = proof.budget(self, deadline)?;
                let context = self.context.clone();
                self.owner.run(Dispatch::Observation, deadline, move || {
                    Ok(repair_readonly_diagnostic(|| {
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
                        let runtime_canonical =
                            runtime.canonical_dos_path(&context.security, &budget)?;
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
                        Ok(Some(AgentObservation(Arc::new(AgentObservationData {
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
                        }))))
                    }))
                })
            }
        }
    }
    #[cfg(not(test))]
    pub(crate) use repair_probe::{RepairProbe, RepairSelection, RepairTaskBinding};

    // A6b uses distinct payload-repair documents and permits. The a6 metadata driver,
    // its record and its exclusion remain unchanged.
    #[cfg(not(test))]
    pub(crate) mod payload_repair_io {
        use super::super::super::{
            payload::{
                inventory::{ApprovedInventory, ApprovedPe, PayloadRole},
                recovery::{FileStamp, ImageObservation, OriginalLeaf},
            },
            repair::{
                RepairObservation,
                payload::{self as controller, PayloadRepairDecision},
                payload_record::{
                    PayloadRepairCatalog, PayloadRepairCatalogPhase, PayloadRepairPhase,
                    PayloadRepairProcess, PayloadRepairPublicationIntent as Intent,
                    PayloadRepairPublicationObservation as Observation,
                    PayloadRepairPublicationPhase as PublicationPhase,
                    PayloadRepairPublicationStamp as Stamp,
                    PayloadRepairPublicationTarget as Target, PayloadRepairRecord,
                    PayloadRepairSelection, PayloadRepairTask, recover_payload_repair_publication,
                    retired_cleanup_settled, source_can_cancel, terminal_publication_admitted,
                },
            },
        };
        use super::*;

        fn read(
            parent: &Anchor,
            context: &Context,
            name: records::RecordName,
            budget: &Deadline,
        ) -> NativeResult<Option<(FileIdentity, Vec<u8>)>> {
            let value = parent.read_private(
                &name.file_name()?,
                &context.security,
                files::MAX_RECORD_BYTES,
                budget,
            )?;
            if let Some((_, bytes)) = &value {
                records::validate_for(&name, bytes)?;
            }
            budget.check()?;
            Ok(value)
        }
        fn pending(
            parent: &Anchor,
            context: &Context,
            budget: &Deadline,
        ) -> NativeResult<Option<(FileIdentity, Vec<u8>)>> {
            parent.read_private(
                &records::RecordName::RepairPayloadPending.file_name()?,
                &context.security,
                files::MAX_RECORD_BYTES,
                budget,
            )
        }
        fn observed(value: &Option<(FileIdentity, Vec<u8>)>) -> NativeResult<Option<Stamp>> {
            value
                .as_ref()
                .map(|(id, bytes)| Stamp::new(FileStamp::from(*id), bytes))
                .transpose()
        }
        type PublicationImage = (FileIdentity, Vec<u8>, Intent);
        fn publication(
            parent: &Anchor,
            context: &Context,
            budget: &Deadline,
        ) -> NativeResult<(Option<PublicationImage>, Observation)> {
            let raw = read(
                parent,
                context,
                records::RecordName::RepairPayloadPublicationIntent,
                budget,
            )?;
            let Some((id, bytes)) = raw else {
                if pending(parent, context, budget)?.is_some() {
                    return Err(NativeError::OutcomeUnknown);
                }
                return Ok((None, Observation::Published));
            };
            let intent = Intent::decode(&bytes)?;
            let current = read(parent, context, intent.target().name(), budget)?;
            let waiting = pending(parent, context, budget)?;
            let state = recover_payload_repair_publication(
                &intent,
                observed(&current)?,
                observed(&waiting)?,
            );
            Ok((Some((id, bytes, intent)), state))
        }
        fn clean_publication(
            parent: &Anchor,
            context: &Context,
            budget: &Deadline,
        ) -> NativeResult<()> {
            let (intent, state) = publication(parent, context, budget)?;
            if state != Observation::Published
                || intent
                    .as_ref()
                    .is_some_and(|(_, _, intent)| intent.phase() != PublicationPhase::Published)
            {
                return Err(NativeError::OutcomeUnknown);
            }
            budget.check()
        }
        fn catalog(
            parent: &Anchor,
            context: &Context,
            budget: &Deadline,
        ) -> NativeResult<PayloadRepairCatalog> {
            read(
                parent,
                context,
                records::RecordName::RepairPayloadCatalog,
                budget,
            )?
            .map(|(_, bytes)| PayloadRepairCatalog::decode(&bytes))
            .transpose()
            .map(|value| value.unwrap_or_default())
        }
        fn check_slot(
            parent: &Anchor,
            context: &Context,
            record: &PayloadRepairRecord,
            budget: &Deadline,
        ) -> NativeResult<()> {
            let slot = record.slot().ok_or(NativeError::Foreign)?;
            let history = catalog(parent, context, budget)?;
            match history.get(slot) {
                Some(entry) => entry.matches(record),
                None if record.phase() == PayloadRepairPhase::Cancelled => Ok(()),
                None => Err(NativeError::Foreign),
            }
        }
        fn refuse_old_operations(
            context: &Context,
            lease: &LockState,
            budget: &Deadline,
        ) -> NativeResult<()> {
            validate_payload_lock(context, lease, budget)?;
            super::repair_archive::refuse_active_repair(context, lease, budget)?;
            use super::super::super::payload::recovery::{
                FileRecoveryCursor, FileRecoveryJournal, OuterPhase, OuterUpgradeRecord,
                StageCatalog,
            };
            if let Some((_, bytes)) = read(
                &lease.parent,
                context,
                records::RecordName::StageCatalog,
                budget,
            )? {
                let catalog: StageCatalog =
                    records::record_data(&records::RecordName::StageCatalog, &bytes)?;
                catalog.validate()?;
                if catalog.active.is_some() {
                    return Err(NativeError::OutcomeUnknown);
                }
            }
            if let Some((_, bytes)) = read(
                &lease.parent,
                context,
                records::RecordName::OuterUpgrade,
                budget,
            )? && !matches!(
                OuterUpgradeRecord::decode(&bytes)?.phase(),
                OuterPhase::Complete | OuterPhase::Cancelled
            ) {
                return Err(NativeError::OutcomeUnknown);
            }
            if let Some((_, bytes)) = read(
                &lease.parent,
                context,
                records::RecordName::FileRecovery,
                budget,
            )? && FileRecoveryJournal::decode(&bytes)?.cursor() != FileRecoveryCursor::Retired
            {
                return Err(NativeError::OutcomeUnknown);
            }
            if let Some((_, bytes)) =
                read(&lease.parent, context, records::RecordName::Removal, budget)?
                && super::super::super::removal::RemovalRecord::decode(&bytes)?.cursor()
                    != super::super::super::removal::RemovalCursor::Retired
            {
                return Err(NativeError::OutcomeUnknown);
            }
            budget.check()
        }
        fn check_repair_record(
            context: &Context,
            lease: &LockState,
            expected: &[u8],
            budget: &Deadline,
        ) -> NativeResult<PayloadRepairRecord> {
            validate_payload_lock(context, lease, budget)?;
            clean_publication(&lease.parent, context, budget)?;
            let (_, actual) = read(
                &lease.parent,
                context,
                records::RecordName::RepairPayload,
                budget,
            )?
            .ok_or(NativeError::Missing)?;
            if actual != expected {
                return Err(NativeError::Foreign);
            }
            let record = PayloadRepairRecord::decode(&actual)?;
            record.context().matches(&context.target.identity)?;
            check_slot(&lease.parent, context, &record, budget)?;
            Ok(record)
        }
        fn native_tree(
            record: &PayloadRepairRecord,
            tree: &super::super::supervisor_owner::RetainedTreeCompletion,
            budget: &Deadline,
        ) -> NativeResult<()> {
            if tree.operation() != record.operation()
                || tree.original_instance() != record.original_generation().instance
            {
                return Err(NativeError::Foreign);
            }
            tree.reverify(budget)
        }
        fn check_terminal_repair_history(
            context: &Context,
            lease: &LockState,
            expected: &[u8],
            budget: &Deadline,
        ) -> NativeResult<(PayloadRepairRecord, bool)> {
            // This FILE-only history check cannot admit a live repair, tree or Start. The live
            // check above still requires the exact logon/session, including after failures.
            refuse_old_operations(context, lease, budget)?;
            let (_, actual) = read(
                &lease.parent,
                context,
                records::RecordName::RepairPayload,
                budget,
            )?
            .ok_or(NativeError::Missing)?;
            if actual != expected {
                return Err(NativeError::Foreign);
            }
            let record = PayloadRepairRecord::decode(&actual)?;
            record.context().same_user(&context.target.identity)?;
            if !matches!(
                record.phase(),
                PayloadRepairPhase::Complete | PayloadRepairPhase::Retired
            ) {
                return Err(NativeError::Foreign);
            }
            check_slot(&lease.parent, context, &record, budget)?;
            let mut retired = record.clone();
            retired.advance(PayloadRepairPhase::Retired)?;
            let (intent, state) = publication(&lease.parent, context, budget)?;
            let raw_intent = intent.as_ref().map(|(_, _, intent)| intent);
            let mut history = catalog(&lease.parent, context, budget)?;
            if retired_cleanup_settled(&record, &history, raw_intent, state)? {
                return Ok((record, true));
            }
            history.retire(retired.slot().ok_or(NativeError::Foreign)?, &retired)?;
            if !terminal_publication_admitted(
                raw_intent,
                state,
                record.operation(),
                Target::Record,
                &retired.encode()?,
            ) && !(record.phase() == PayloadRepairPhase::Retired
                && terminal_publication_admitted(
                    raw_intent,
                    state,
                    record.operation(),
                    Target::Catalog,
                    &history.encode()?,
                ))
            {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok((record, false))
        }
        /// Additional closed selection exclusion. Record facts alone never permit another
        /// operation to preempt an active or uncertain repair.
        pub(super) fn refuse_active(
            context: &Context,
            lease: &LockState,
            budget: &Deadline,
        ) -> NativeResult<()> {
            validate_payload_lock(context, lease, budget)?;
            clean_publication(&lease.parent, context, budget)?;
            let current = read(
                &lease.parent,
                context,
                records::RecordName::RepairPayload,
                budget,
            )?;
            let history = catalog(&lease.parent, context, budget)?;
            if (0..3)
                .filter_map(|slot| history.get(slot))
                .any(|entry| entry.phase() == PayloadRepairCatalogPhase::Active)
            {
                return Err(NativeError::OutcomeUnknown);
            }
            if let Some((_, bytes)) = current {
                let record = PayloadRepairRecord::decode(&bytes)?;
                record.context().same_user(&context.target.identity)?;
                if !matches!(
                    record.phase(),
                    PayloadRepairPhase::Complete
                        | PayloadRepairPhase::Retired
                        | PayloadRepairPhase::Cancelled
                ) {
                    return Err(NativeError::OutcomeUnknown);
                }
                check_slot(&lease.parent, context, &record, budget)?;
            }
            budget.check()
        }
        fn persist_intent(
            parent: &Anchor,
            context: &Context,
            current: &mut Option<(FileIdentity, Vec<u8>, Intent)>,
            intent: Intent,
            budget: &Deadline,
            change: &Change,
        ) -> NativeResult<()> {
            let bytes = intent.encode()?;
            let previous = current
                .as_ref()
                .map(|(id, bytes, _)| (*id, bytes.as_slice()));
            change.reached();
            let id = parent.write_payload_repair_publication_intent(
                previous,
                &bytes,
                &context.security,
                budget,
            )?;
            *current = Some((id, bytes, intent));
            Ok(())
        }
        /// Exact durable fixed intent and pending stamp precede an ordinary same-parent
        /// replacement. A torn fixed intent is Unknown; no temporary-name search repairs it.
        fn publish_fixed(
            context: &Context,
            lease: &LockState,
            operation: [u8; 16],
            target: Target,
            bytes: &[u8],
            budget: &Deadline,
            change: &Change,
        ) -> NativeResult<()> {
            validate_payload_lock(context, lease, budget)?;
            let parent = &lease.parent;
            if target == Target::Catalog {
                let old = catalog(parent, context, budget)?;
                let next = PayloadRepairCatalog::decode(bytes)?;
                for slot in 0..3 {
                    if old.get(slot).is_some() && next.get(slot).is_none() {
                        let (_, raw) =
                            read(parent, context, records::RecordName::RepairPayload, budget)?
                                .ok_or(NativeError::Missing)?;
                        let cancelled = PayloadRepairRecord::decode(&raw)?;
                        if cancelled.phase() != PayloadRepairPhase::Cancelled
                            || cancelled.slot() != Some(slot)
                            || cancelled.operation() != operation
                        {
                            return Err(NativeError::Foreign);
                        }
                        old.get(slot)
                            .ok_or(NativeError::Foreign)?
                            .matches(&cancelled)?;
                    }
                }
                old.publication_successor(&next)?;
            }
            let (mut current, state) = publication(parent, context, budget)?;
            let same = current
                .as_ref()
                .is_some_and(|(_, _, intent)| intent.matches_request(operation, target, bytes));
            if same && state == Observation::Published {
                if current
                    .as_ref()
                    .is_some_and(|(_, _, intent)| intent.phase() != PublicationPhase::Published)
                {
                    let mut intent = current.as_ref().ok_or(NativeError::Foreign)?.2.clone();
                    intent.published()?;
                    persist_intent(parent, context, &mut current, intent, budget, change)?;
                }
                return Ok(());
            }
            if !same {
                if state != Observation::Published
                    || current
                        .as_ref()
                        .is_some_and(|(_, _, intent)| intent.phase() != PublicationPhase::Published)
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                let prior = read(parent, context, target.name(), budget)?;
                let intent = Intent::new(operation, target, observed(&prior)?, bytes)?;
                persist_intent(parent, context, &mut current, intent, budget, change)?;
            } else if state == Observation::Unknown {
                return Err(NativeError::OutcomeUnknown);
            }
            let mut intent = current.as_ref().ok_or(NativeError::Foreign)?.2.clone();
            if intent.phase() == PublicationPhase::Preparing {
                if pending(parent, context, budget)?.is_some() {
                    return Err(NativeError::OutcomeUnknown);
                }
                change.reached();
                let id = parent.create_payload_repair_pending(bytes, &context.security, budget)?;
                intent.pending_ready(Stamp::new(id.into(), bytes)?)?;
                persist_intent(
                    parent,
                    context,
                    &mut current,
                    intent.clone(),
                    budget,
                    change,
                )?;
            }
            if intent.phase() == PublicationPhase::PendingReady {
                intent.replace_intent()?;
                persist_intent(
                    parent,
                    context,
                    &mut current,
                    intent.clone(),
                    budget,
                    change,
                )?;
            }
            if intent.phase() != PublicationPhase::ReplaceIntent {
                return Err(NativeError::OutcomeUnknown);
            }
            let prior = read(parent, context, target.name(), budget)?;
            let waiting = pending(parent, context, budget)?;
            match recover_payload_repair_publication(
                &intent,
                observed(&prior)?,
                observed(&waiting)?,
            ) {
                Observation::PendingReady => {
                    let stamp = intent.pending().ok_or(NativeError::Foreign)?.identity();
                    change.reached();
                    parent.publish_payload_repair_pending(
                        &target.name().file_name()?,
                        FileIdentity {
                            volume: stamp.volume,
                            file: stamp.file,
                        },
                        bytes,
                        prior.as_ref().map(|(id, bytes)| (*id, bytes.as_slice())),
                        &context.security,
                        budget,
                    )?;
                }
                Observation::Published => {}
                _ => return Err(NativeError::OutcomeUnknown),
            }
            intent.published()?;
            persist_intent(parent, context, &mut current, intent, budget, change)?;
            clean_publication(parent, context, budget)
        }
        fn repair_copy_parent(
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
                let repairs = ensure_payload_child(&runtime, "repair", context, budget, change)?;
                return Ok(Some(Arc::new(ensure_payload_child(
                    &repairs,
                    &records::hex(&operation),
                    context,
                    budget,
                    change,
                )?)));
            }
            Anchor::open(
                &format!(
                    "{}\\Crosspane\\runtime\\repair\\{}",
                    context.target.paths.local(),
                    records::hex(&operation)
                ),
                &context.security,
                true,
                budget,
            )
            .map(|value| value.map(Arc::new))
        }
        /// Completion-role image identity only. This cannot create an ApprovedPe or Run permit.
        pub(crate) struct RepairPeerImage(Arc<RepairPeerData>);
        struct RepairPeerData {
            io: Arc<WindowsNativeIo>,
            record: PayloadRepairRecord,
            parent: Arc<Anchor>,
            image: native::ImageData,
        }
        impl RepairPeerImage {
            pub(crate) fn identity(&self) -> FileIdentity {
                self.0.image.identity
            }
            pub(crate) fn facts(&self) -> &super::super::super::payload::inventory::PeFacts {
                &self.0.image.facts
            }
            pub(crate) fn canonical_dos_path(&self) -> &str {
                &self.0.image.canonical
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                budget: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.0.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                let current = io
                    .read_payload_repair(proof, budget)?
                    .ok_or(NativeError::Missing)?;
                if !current.same_selection(&self.0.record)
                    || current.keeper().image()
                        != Some(&ImageObservation {
                            identity: self.identity().into(),
                            facts: self.facts().clone(),
                        })
                {
                    return Err(NativeError::Foreign);
                }
                let pin = self.0.clone();
                let context = io.context.clone();
                let checked = proof.budget(io, budget)?;
                io.owner.run(Dispatch::Observation, budget, move || {
                    context.validate(&checked)?;
                    pin.parent.revalidate(&context.security, true, &checked)?;
                    let fresh = pin.parent.open_image(
                        "keeper-copy.exe",
                        true,
                        &pin.image.facts.version,
                        &context.security,
                        &checked,
                    )?;
                    if fresh.identity != pin.image.identity || fresh.facts != pin.image.facts {
                        return Err(NativeError::Foreign);
                    }
                    checked.check()
                })
            }
        }
        pub(crate) struct RepairKeeperSelection {
            io: Arc<WindowsNativeIo>,
            module: SelfImagePin,
            own: process::own::OwnProcessIdentity,
            image: RepairPeerImage,
            record: PayloadRepairRecord,
        }
        impl RepairKeeperSelection {
            pub(crate) fn operation(&self) -> [u8; 16] {
                self.record.operation()
            }
            pub(crate) fn record(&self) -> &PayloadRepairRecord {
                &self.record
            }
            pub(crate) fn module(&self) -> &SelfImagePin {
                &self.module
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                budget: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                self.module.reverify(io, proof, budget)?;
                self.own.reverify(budget)?;
                self.image.reverify(io, proof, budget)?;
                let current = io
                    .read_payload_repair(proof, budget)?
                    .ok_or(NativeError::Missing)?;
                if !self.record.same_selection(&current)
                    || current.keeper().child()
                        != Some(PayloadRepairProcess::new(
                            self.own.pid(),
                            self.own.creation(),
                        )?)
                    || self.module.identity() != self.image.identity()
                    || self.module.facts() != self.image.facts()
                    || process::literal_path(self.module.canonical_dos_path())?
                        != process::literal_path(self.image.canonical_dos_path())?
                {
                    return Err(NativeError::Foreign);
                }
                budget.check()
            }
        }
        pub(crate) struct RepairCompletionAdmission {
            selection: RepairKeeperSelection,
        }
        impl RepairCompletionAdmission {
            pub(crate) fn operation(&self) -> [u8; 16] {
                self.selection.operation()
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                budget: &Deadline,
            ) -> NativeResult<()> {
                self.selection.reverify(io, proof, budget)?;
                let current = io
                    .read_payload_repair(proof, budget)?
                    .ok_or(NativeError::Missing)?;
                if current.operation() != self.operation()
                    || current.phase() != PayloadRepairPhase::StopIntent
                {
                    return Err(NativeError::Foreign);
                }
                current.context().matches(io.target().identity())?;
                budget.check()
            }
        }
        pub(crate) mod keeper {
            use super::super::super::super::payload::ApprovedOuterSources;
            use super::*;
            use serde::{Deserialize, Serialize};
            use std::{
                os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
                sync::OnceLock,
            };
            use tokio::{
                io::{AsyncRead, AsyncWrite, ReadBuf},
                net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions},
            };
            use windows_sys::Win32::{
                Foundation::*,
                Security::{Authorization::*, *},
                System::{JobObjects::IsProcessInJob, Pipes::*, Threading::*},
            };
            const CHUNK: usize = 64 * 1024;
            fn tuple(process: &OwnedHandle) -> NativeResult<PayloadRepairProcess> {
                let (mut creation, mut exit, mut kernel, mut user) = (
                    FILETIME::default(),
                    FILETIME::default(),
                    FILETIME::default(),
                    FILETIME::default(),
                );
                // SAFETY: query exactly the actual retained process, with complete time outputs.
                if unsafe {
                    GetProcessTimes(
                        process.as_raw_handle(),
                        &mut creation,
                        &mut exit,
                        &mut kernel,
                        &mut user,
                    )
                } == 0
                {
                    return Err(NativeError::Unavailable);
                }
                // SAFETY: query this retained process handle, never select or reopen a PID.
                let pid = unsafe { GetProcessId(process.as_raw_handle()) };
                PayloadRepairProcess::new(
                    pid,
                    (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
                )
            }
            fn exited(process: &OwnedHandle) -> bool {
                // SAFETY: nonblocking observation of the same actually retained process object.
                (unsafe { WaitForSingleObject(process.as_raw_handle(), 0) }) == WAIT_OBJECT_0
            }
            pub(crate) struct RepairParent {
                io: Arc<WindowsNativeIo>,
                process: Arc<OwnedHandle>,
                original: PayloadRepairProcess,
                operation: [u8; 16],
            }
            impl RepairParent {
                fn inherited(
                    io: Arc<WindowsNativeIo>,
                    record: &PayloadRepairRecord,
                    proof: &SupportProof,
                    budget: &Deadline,
                ) -> NativeResult<Self> {
                    proof.check(&io, budget)?;
                    let value = record
                        .keeper()
                        .inherited_parent_handle()
                        .ok_or(NativeError::Foreign)?;
                    if value < 4
                        || value > usize::MAX as u64
                        || value >= usize::MAX as u64 - 3
                        || !value.is_multiple_of(4)
                    {
                        return Err(NativeError::Foreign);
                    }
                    let raw = value as usize as std::os::windows::io::RawHandle;
                    let mut flags = 0;
                    // SAFETY: validate the explicitly inherited bounded handle before adoption.
                    if unsafe { GetHandleInformation(raw, &mut flags) } == 0
                        || flags & HANDLE_FLAG_INHERIT == 0
                    {
                        return Err(NativeError::Foreign);
                    }
                    let mut duplicate = std::ptr::null_mut();
                    // SAFETY: duplicate only the actual inherited parent into THIS process with
                    // query/synchronize rights, noninheritable; no PID lookup or broader rights.
                    if unsafe {
                        DuplicateHandle(
                            GetCurrentProcess(),
                            raw,
                            GetCurrentProcess(),
                            &mut duplicate,
                            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                            0,
                            0,
                        )
                    } == 0
                    {
                        return Err(NativeError::Foreign);
                    }
                    // SAFETY: successful DuplicateHandle transferred one local owned handle.
                    let process = Arc::new(unsafe { OwnedHandle::from_raw_handle(duplicate) });
                    let original = tuple(&process)?;
                    if Some(original) != record.keeper().parent()
                        || identity::native::observe_process(&process)? != *io.target().identity()
                    {
                        return Err(NativeError::Foreign);
                    }
                    // SAFETY: the inherited handle is now owned by its restricted duplicate;
                    // no later effect uses the serialized correlation as an owning handle.
                    if unsafe { CloseHandle(raw) } == 0 {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    let cap = Self {
                        io,
                        process,
                        original,
                        operation: record.operation(),
                    };
                    cap.reverify(&cap.io, proof, cap.operation, budget)?;
                    Ok(cap)
                }
                pub(crate) fn retained_process(&self) -> &Arc<OwnedHandle> {
                    &self.process
                }
                pub(crate) fn reverify(
                    &self,
                    io: &WindowsNativeIo,
                    proof: &SupportProof,
                    operation: [u8; 16],
                    budget: &Deadline,
                ) -> NativeResult<()> {
                    if !std::ptr::eq(io, self.io.as_ref()) || operation != self.operation {
                        return Err(NativeError::Foreign);
                    }
                    proof.check(io, budget)?;
                    if tuple(&self.process)? != self.original {
                        return Err(NativeError::Foreign);
                    }
                    budget.check()
                }
                pub(crate) fn has_exited(&self, budget: &Deadline) -> NativeResult<bool> {
                    self.reverify(
                        &self.io,
                        &self.io.admit_support(budget)?,
                        self.operation,
                        budget,
                    )?;
                    Ok(exited(&self.process))
                }
            }
            fn endpoint(io: &WindowsNativeIo) -> String {
                format!(
                    r"\\.\pipe\Crosspane-payload-repair-{}-{}",
                    io.target().identity().user.sddl(),
                    io.target().identity().session
                )
            }
            struct Descriptor(*mut std::ffi::c_void);
            impl Drop for Descriptor {
                fn drop(&mut self) {
                    // SAFETY: only the successful SDDL allocation is owned here.
                    unsafe {
                        LocalFree(self.0);
                    }
                }
            }
            pub(crate) struct RepairKeeperLease {
                io: Arc<WindowsNativeIo>,
                namespace: Arc<OwnedHandle>,
                own: process::own::OwnProcessIdentity,
                operation: [u8; 16],
            }
            impl RepairKeeperLease {
                pub(crate) fn reserve(
                    io: Arc<WindowsNativeIo>,
                    proof: &SupportProof,
                    operation: [u8; 16],
                    budget: &Deadline,
                ) -> NativeResult<(NamedPipeServer, Arc<Self>)> {
                    proof.check(&io, budget)?;
                    if operation == [0; 16] {
                        return Err(NativeError::Invalid);
                    }
                    let own = io.own_process_identity(proof, budget)?;
                    let user = io.target().identity().user.sddl();
                    let logon = io.target().identity().logon.sddl();
                    let text = native::wide(&format!(
                        "O:{user}G:{user}D:P(A;;GA;;;{user})(A;;GRGW;;;{logon})"
                    ))?;
                    let mut raw = std::ptr::null_mut();
                    // SAFETY: bounded NUL SDDL and complete descriptor output.
                    if unsafe {
                        ConvertStringSecurityDescriptorToSecurityDescriptorW(
                            text.as_ptr(),
                            SDDL_REVISION_1,
                            &mut raw,
                            std::ptr::null_mut(),
                        )
                    } == 0
                    {
                        return Err(NativeError::Unavailable);
                    }
                    let descriptor = Descriptor(raw);
                    let attrs = SECURITY_ATTRIBUTES {
                        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                        lpSecurityDescriptor: descriptor.0,
                        bInheritHandle: 0,
                    };
                    // SAFETY: protected current user/logon DACL stays live through first-instance
                    // local-only creation. This is genuine kernel exclusivity, never record facts.
                    let pipe = unsafe {
                        ServerOptions::new()
                            .first_pipe_instance(true)
                            .reject_remote_clients(true)
                            .max_instances(1)
                            .create_with_security_attributes_raw(
                                endpoint(&io),
                                (&attrs as *const SECURITY_ATTRIBUTES).cast_mut().cast(),
                            )
                    }
                    .map_err(|_| NativeError::Busy)?;
                    let mut raw = std::ptr::null_mut();
                    // SAFETY: noninheritable duplicate of our genuine first-instance server object.
                    if unsafe {
                        DuplicateHandle(
                            GetCurrentProcess(),
                            pipe.as_raw_handle(),
                            GetCurrentProcess(),
                            &mut raw,
                            0,
                            0,
                            DUPLICATE_SAME_ACCESS,
                        )
                    } == 0
                    {
                        return Err(NativeError::Unavailable);
                    }
                    // SAFETY: successful duplication transfers exactly one local handle.
                    let namespace = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
                    let cap = Arc::new(Self {
                        io,
                        namespace,
                        own,
                        operation,
                    });
                    cap.reverify(&cap.io, proof, operation, budget)?;
                    Ok((pipe, cap))
                }
                pub(crate) fn reverify(
                    &self,
                    io: &WindowsNativeIo,
                    proof: &SupportProof,
                    operation: [u8; 16],
                    budget: &Deadline,
                ) -> NativeResult<()> {
                    proof.check(io, budget)?;
                    self.reverify_on_owner(io, proof, operation, budget)
                }
                pub(super) fn reverify_on_owner(
                    &self,
                    io: &WindowsNativeIo,
                    proof: &SupportProof,
                    operation: [u8; 16],
                    budget: &Deadline,
                ) -> NativeResult<()> {
                    if !std::ptr::eq(io, self.io.as_ref()) || self.operation != operation {
                        return Err(NativeError::Foreign);
                    }
                    proof.binding(io, budget)?;
                    io.context.validate(budget)?;
                    self.own.reverify(budget)?;
                    let mut flags = 0;
                    // SAFETY: query only this retained original first-instance object.
                    if unsafe {
                        GetNamedPipeInfo(
                            self.namespace.as_raw_handle(),
                            &mut flags,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                        )
                    } == 0
                    {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    budget.check()
                }
            }
            #[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
            #[serde(rename_all = "kebab-case")]
            pub(crate) enum Method {
                Ready,
                Commit,
                Status,
            }
            #[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
            #[serde(rename_all = "kebab-case")]
            pub(crate) enum State {
                Ready,
                Committed,
                Complete,
                Retained,
                ReinstallRequired,
            }
            #[derive(Serialize, Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Frame {
                schema_version: u32,
                operation: [u8; 16],
                nonce: [u8; 16],
                method: Method,
            }
            #[derive(Serialize, Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Reply {
                schema_version: u32,
                operation: [u8; 16],
                nonce: [u8; 16],
                state: State,
            }
            fn runtime() -> NativeResult<tokio::runtime::Runtime> {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
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
            async fn read_bounded<R: AsyncRead + Unpin>(
                pipe: &mut R,
                bytes: &mut [u8],
                budget: &Deadline,
            ) -> NativeResult<()> {
                tokio::time::timeout(
                    Duration::from_millis(budget.remaining_ms()?),
                    read_exact(pipe, bytes),
                )
                .await
                .map_err(|_| NativeError::OutcomeUnknown)?
                .map_err(|_| NativeError::Unavailable)?;
                budget.check()
            }
            async fn write_bounded<W: AsyncWrite + Unpin>(
                pipe: &mut W,
                bytes: &[u8],
                budget: &Deadline,
            ) -> NativeResult<()> {
                tokio::time::timeout(
                    Duration::from_millis(budget.remaining_ms()?),
                    write_all(pipe, bytes),
                )
                .await
                .map_err(|_| NativeError::OutcomeUnknown)?
                .map_err(|_| NativeError::Unavailable)?;
                budget.check()
            }
            async fn read_frame<T: serde::de::DeserializeOwned, R: AsyncRead + Unpin>(
                stream: &mut R,
                budget: &Deadline,
            ) -> NativeResult<T> {
                let mut prefix = [0; 4];
                read_bounded(stream, &mut prefix, budget).await?;
                let length = u32::from_le_bytes(prefix) as usize;
                if length == 0 || length > CHUNK {
                    return Err(NativeError::Oversize);
                }
                let mut bytes = vec![0; length];
                read_bounded(stream, &mut bytes, budget).await?;
                serde_json::from_slice(&bytes).map_err(|_| NativeError::Foreign)
            }
            async fn write_frame<T: Serialize, W: AsyncWrite + Unpin>(
                stream: &mut W,
                value: &T,
                budget: &Deadline,
            ) -> NativeResult<()> {
                let bytes = serde_json::to_vec(value).map_err(|_| NativeError::Invalid)?;
                if bytes.is_empty() || bytes.len() > CHUNK {
                    return Err(NativeError::Oversize);
                }
                write_bounded(stream, &(bytes.len() as u32).to_le_bytes(), budget).await?;
                write_bounded(stream, &bytes, budget).await
            }
            /// Never decoded or reconstructed. Only an actual authenticated atomic commit
            /// transfers resident Stop/file/start ownership to this genuine copied process.
            pub(crate) struct RepairCommit {
                io: Arc<WindowsNativeIo>,
                selection: Arc<RepairKeeperSelection>,
                namespace: Arc<RepairKeeperLease>,
                parent: Arc<RepairParent>,
                sources: Arc<ApprovedOuterSources>,
            }
            impl RepairCommit {
                pub(crate) fn selection(&self) -> &Arc<RepairKeeperSelection> {
                    &self.selection
                }
                pub(crate) fn sources(&self) -> &Arc<ApprovedOuterSources> {
                    &self.sources
                }
                pub(crate) fn reverify(&self, budget: &Deadline) -> NativeResult<()> {
                    let proof = self.io.admit_support(budget)?;
                    self.selection.reverify(&self.io, &proof, budget)?;
                    self.namespace.reverify(
                        &self.io,
                        &proof,
                        self.selection.operation(),
                        budget,
                    )?;
                    self.parent
                        .reverify(&self.io, &proof, self.selection.operation(), budget)
                }
                pub(crate) fn parent_settled(&self, budget: &Deadline) -> NativeResult<()> {
                    self.reverify(budget)?;
                    if !self.parent.has_exited(budget)? {
                        return Err(NativeError::Busy);
                    }
                    Ok(())
                }
            }
            #[derive(Default)]
            struct LaunchState {
                process: Option<Arc<OwnedHandle>>,
                thread: Option<Arc<OwnedHandle>>,
                valid: bool,
                call_live: bool,
                abandoned: bool,
                resume_attempted: bool,
                ready_attempted: bool,
                commit_attempted: bool,
                // Positive reply to this child's exact authenticated Commit, not an attempt bit.
                commit_confirmed: bool,
                workers: Vec<std::thread::JoinHandle<()>>,
            }
            struct LaunchOwner {
                io: Arc<WindowsNativeIo>,
                operation: [u8; 16],
                image: Arc<OpenedPe>,
                parent: Arc<OwnedHandle>,
                state: Mutex<LaunchState>,
            }
            static LAUNCH: OnceLock<Mutex<Option<Arc<LaunchOwner>>>> = OnceLock::new();
            #[derive(Clone)]
            pub(crate) struct RepairChild {
                owner: Arc<LaunchOwner>,
            }
            fn abandon_unresumed(owner: &LaunchOwner) {
                let mut state = owner
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                state.abandoned = true;
                if !state.resume_attempted
                    && let Some(process) = &state.process
                {
                    // SAFETY: exact actual child we created suspended; the mutex prevents
                    // any Resume reservation from racing this cleanup. Never a PID lookup.
                    unsafe {
                        TerminateProcess(process.as_raw_handle(), 1);
                    }
                }
            }
            /// Actual source/worker settlement only. No record can construct this seal.
            pub(crate) struct SourceCancellation {
                io: Arc<WindowsNativeIo>,
                operation: [u8; 16],
                source: process::own::OwnProcessIdentity,
                child: Option<Arc<OwnedHandle>>,
            }
            impl SourceCancellation {
                pub(super) fn reverify(
                    &self,
                    io: &WindowsNativeIo,
                    operation: [u8; 16],
                    budget: &Deadline,
                ) -> NativeResult<()> {
                    if !std::ptr::eq(io, self.io.as_ref()) || operation != self.operation {
                        return Err(NativeError::Foreign);
                    }
                    self.source.reverify(budget)?;
                    if self.child.as_ref().is_some_and(|child| !exited(child)) {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    budget.check()
                }
            }
            pub(crate) fn cancel_source(
                io: Arc<WindowsNativeIo>,
                operation: [u8; 16],
                budget: &Deadline,
            ) -> NativeResult<SourceCancellation> {
                let source = io.own_process_identity(&io.admit_support(budget)?, budget)?;
                let owner = LAUNCH
                    .get()
                    .map(|slot| {
                        slot.lock()
                            .map_err(|_| NativeError::OutcomeUnknown)
                            .map(|slot| slot.clone())
                    })
                    .transpose()?
                    .flatten();
                let mut child = None;
                if let Some(owner) = &owner {
                    if !Arc::ptr_eq(&owner.io, &io) || owner.operation != operation {
                        return Err(NativeError::Foreign);
                    }
                    {
                        let state = owner
                            .state
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        if state.resume_attempted || state.commit_attempted {
                            return Err(NativeError::OutcomeUnknown);
                        }
                    }
                    abandon_unresumed(owner);
                    loop {
                        budget.check()?;
                        let settled = {
                            let state = owner
                                .state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            if state.resume_attempted || state.commit_attempted {
                                return Err(NativeError::OutcomeUnknown);
                            }
                            !state.call_live
                                && state
                                    .workers
                                    .iter()
                                    .all(std::thread::JoinHandle::is_finished)
                        };
                        if settled {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    let workers = {
                        let mut state = owner
                            .state
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        child = state.process.clone();
                        std::mem::take(&mut state.workers)
                    };
                    for worker in workers {
                        worker.join().map_err(|_| NativeError::OutcomeUnknown)?;
                    }
                    if let Some(process) = &child {
                        // SAFETY: bounded wait on the same known-owned never-resumed child.
                        if unsafe {
                            WaitForSingleObject(
                                process.as_raw_handle(),
                                budget.remaining_ms()?.min(5_000) as u32,
                            )
                        } != WAIT_OBJECT_0
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                    }
                    if let Some(slot) = LAUNCH.get() {
                        let mut slot = slot.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                        if slot
                            .as_ref()
                            .is_some_and(|actual| Arc::ptr_eq(actual, owner))
                        {
                            slot.take();
                        } else {
                            return Err(NativeError::Foreign);
                        }
                    }
                }
                let settled = SourceCancellation {
                    io,
                    operation,
                    source,
                    child,
                };
                settled.reverify(&settled.io, operation, budget)?;
                Ok(settled)
            }
            struct Attributes {
                storage: Vec<usize>,
                handles: Vec<std::os::windows::io::RawHandle>,
            }
            impl Drop for Attributes {
                fn drop(&mut self) {
                    // SAFETY: this wraps only successfully initialized attribute-list storage.
                    unsafe {
                        DeleteProcThreadAttributeList(self.storage.as_mut_ptr().cast());
                    }
                }
            }
            fn attributes(parent: &OwnedHandle) -> NativeResult<Attributes> {
                let mut bytes = 0;
                // SAFETY: documented sizing pass, null list and complete output.
                unsafe {
                    InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes);
                }
                if bytes == 0 || bytes > 65_536 {
                    return Err(NativeError::Unavailable);
                }
                let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
                // SAFETY: aligned storage of the exact returned bounded required size.
                if unsafe {
                    InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut bytes)
                } == 0
                {
                    return Err(NativeError::Unavailable);
                }
                let mut output = Attributes {
                    storage,
                    handles: vec![parent.as_raw_handle()],
                };
                // SAFETY: the sole HANDLE_LIST entry is our actual inheritable retained parent;
                // its storage and object stay live through CreateProcess. No ambient handles inherit.
                if unsafe {
                    UpdateProcThreadAttribute(
                        output.storage.as_mut_ptr().cast(),
                        0,
                        PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                        output.handles.as_mut_ptr().cast(),
                        std::mem::size_of_val(output.handles.as_slice()),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                } == 0
                {
                    return Err(NativeError::Unavailable);
                }
                Ok(output)
            }
            pub(crate) fn prepare(
                io: Arc<WindowsNativeIo>,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RepairMutationPermit,
                image: Arc<OpenedPe>,
                budget: &Deadline,
            ) -> NativeResult<RepairChild> {
                permit.reverify(&io, proof, lock, budget)?;
                image.reverify(&io, proof, budget)?;
                let record = permit.document()?;
                if record.phase() != PayloadRepairPhase::HandoffIntent
                    || record.keeper().image()
                        != Some(&ImageObservation {
                            identity: image.identity().into(),
                            facts: image.approved().facts().clone(),
                        })
                {
                    return Err(NativeError::Foreign);
                }
                let original = io.own_process_identity(proof, budget)?;
                if record.selection().source_process()
                    != PayloadRepairProcess::new(original.pid(), original.creation())?
                {
                    return Err(NativeError::Foreign);
                }
                let mut raw = std::ptr::null_mut();
                // SAFETY: explicitly duplicate OUR actual retained current process with only
                // query/synchronize rights, inheritable for the sole HANDLE_LIST entry.
                if unsafe {
                    DuplicateHandle(
                        GetCurrentProcess(),
                        original.handle().as_raw_handle(),
                        GetCurrentProcess(),
                        &mut raw,
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                        1,
                        0,
                    )
                } == 0
                {
                    return Err(NativeError::Unavailable);
                }
                // SAFETY: successful duplicate transfers exactly one local parent handle.
                let parent = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
                let owner = Arc::new(LaunchOwner {
                    io: io.clone(),
                    operation: record.operation(),
                    image,
                    parent,
                    state: Mutex::new(LaunchState::default()),
                });
                {
                    let mut slot = LAUNCH
                        .get_or_init(|| Mutex::new(None))
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    if slot.is_some() {
                        return Err(NativeError::Busy);
                    }
                    *slot = Some(owner.clone());
                }
                let captured = owner.clone();
                let initial = budget.clone();
                let (send, receive) = std::sync::mpsc::sync_channel(1);
                let worker = std::thread::Builder::new()
                    .name("crosspane-repair-keeper-create".into())
                    .spawn(move || {
                        let result = (|| {
                            let proof = captured.io.admit_support(&initial)?;
                            captured.image.reverify(&captured.io, &proof, &initial)?;
                            let mut attrs = attributes(&captured.parent)?;
                            let application = native::wide(captured.image.canonical_dos_path())?;
                            let (directory, _) = captured
                                .image
                                .canonical_dos_path()
                                .rsplit_once('\\')
                                .ok_or(NativeError::Foreign)?;
                            let directory = native::wide(directory)?;
                            let mut command = native::wide(&format!(
                                "\"{}\" --windows-upgrade-keeper",
                                captured.image.canonical_dos_path()
                            ))?;
                            let startup = STARTUPINFOEXW {
                                StartupInfo: STARTUPINFOW {
                                    cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                                    ..Default::default()
                                },
                                lpAttributeList: attrs.storage.as_mut_ptr().cast(),
                            };
                            let mut output = PROCESS_INFORMATION::default();
                            {
                                let mut state = captured
                                    .state
                                    .lock()
                                    .map_err(|_| NativeError::OutcomeUnknown)?;
                                if state.abandoned {
                                    return Err(NativeError::Cancelled);
                                }
                                initial.check()?;
                                state.call_live = true;
                            }
                            // SAFETY: only an independently pinned fixed repair copy, exact sole argv,
                            // explicit parent HANDLE_LIST, suspended child and breakaway request. Never
                            // change containing job policy or an unrelated process's ownership.
                            let created = unsafe {
                                CreateProcessW(
                                    application.as_ptr(),
                                    command.as_mut_ptr(),
                                    std::ptr::null(),
                                    std::ptr::null(),
                                    1,
                                    CREATE_SUSPENDED
                                        | CREATE_BREAKAWAY_FROM_JOB
                                        | EXTENDED_STARTUPINFO_PRESENT,
                                    std::ptr::null(),
                                    directory.as_ptr(),
                                    &startup.StartupInfo,
                                    &mut output,
                                )
                            };
                            if created == 0 {
                                return Err(NativeError::Unavailable);
                            }
                            // SAFETY: successful CreateProcess transfers these exact two owned outputs.
                            let process =
                                Arc::new(unsafe { OwnedHandle::from_raw_handle(output.hProcess) });
                            // SAFETY: same successful CreateProcess transfers its actual primary thread.
                            let thread =
                                Arc::new(unsafe { OwnedHandle::from_raw_handle(output.hThread) });
                            {
                                // Even a poisoned owner must retain the actual native outputs.
                                // Recovery here grants cleanup only: it never permits Resume.
                                let (mut state, poisoned) = match captured.state.lock() {
                                    Ok(state) => (state, false),
                                    Err(error) => (error.into_inner(), true),
                                };
                                state.process = Some(process.clone());
                                state.thread = Some(thread);
                                if poisoned {
                                    state.abandoned = true;
                                    return Err(NativeError::OutcomeUnknown);
                                }
                            }
                            let mut member = 0;
                            // SAFETY: only our known-owned, never-resumed original child is queried.
                            if unsafe {
                                IsProcessInJob(
                                    process.as_raw_handle(),
                                    std::ptr::null_mut(),
                                    &mut member,
                                )
                            } == 0
                                || member != 0
                            {
                                return Err(NativeError::Unsupported);
                            }
                            if identity::native::observe_process(&process)?
                                != *captured.io.target().identity()
                            {
                                return Err(NativeError::Foreign);
                            }
                            tuple(&process)?;
                            initial.check()?;
                            captured
                                .state
                                .lock()
                                .map_err(|_| NativeError::OutcomeUnknown)?
                                .valid = true;
                            Ok(())
                        })();
                        let abandoned = {
                            let mut state = captured
                                .state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            state.call_live = false;
                            state.abandoned
                        };
                        if result.is_err() || abandoned {
                            // Positive cleanup is limited to this actually created, NEVER-resumed
                            // child. Nothing is selected by PID or terminated after a Resume attempt.
                            let cleanup = Deadline::new(
                                5_000,
                                captured.io.bound_clock(),
                                Cancellation::default(),
                            );
                            let process = {
                                let state = captured
                                    .state
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner());
                                if state.resume_attempted {
                                    None
                                } else {
                                    state.process.clone()
                                }
                            };
                            if let (Ok(cleanup), Some(process)) = (cleanup, process)
                                && cleanup.check().is_ok()
                            {
                                // SAFETY: exact child this process created suspended and never resumed.
                                let terminated =
                                    unsafe { TerminateProcess(process.as_raw_handle(), 1) };
                                if terminated != 0 {
                                    // SAFETY: bounded settlement of that same known-owned child only.
                                    unsafe {
                                        WaitForSingleObject(
                                            process.as_raw_handle(),
                                            cleanup.remaining_ms().unwrap_or(0).min(5_000) as u32,
                                        );
                                    }
                                }
                            }
                        }
                        let _ = send.try_send(result);
                    })
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                let registered = {
                    let (mut state, poisoned) = match owner.state.lock() {
                        Ok(state) => (state, false),
                        Err(error) => (error.into_inner(), true),
                    };
                    state.workers.push(worker);
                    if poisoned {
                        state.abandoned = true;
                    }
                    !poisoned
                };
                let delivered = (|| {
                    if !registered {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    let wait = budget.remaining_ms()?;
                    match receive.recv_timeout(Duration::from_millis(wait)) {
                        Ok(Ok(())) => budget.check(),
                        _ => Err(NativeError::OutcomeUnknown),
                    }
                })();
                match delivered {
                    Ok(()) => Ok(RepairChild { owner }),
                    Err(_) => {
                        // Includes post-success delivery, exhausted budget and poisoned
                        // registration. Cleanup is atomic with every Resume reservation.
                        abandon_unresumed(&owner);
                        Err(NativeError::OutcomeUnknown)
                    }
                }
            }
            impl RepairChild {
                fn process(&self) -> NativeResult<Arc<OwnedHandle>> {
                    let state = self
                        .owner
                        .state
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    if !state.valid || state.call_live {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    state
                        .process
                        .as_ref()
                        .cloned()
                        .ok_or(NativeError::OutcomeUnknown)
                }
                pub(crate) fn facts(
                    &self,
                ) -> NativeResult<(PayloadRepairProcess, PayloadRepairProcess, u64)>
                {
                    Ok((
                        tuple(&self.owner.parent)?,
                        tuple(self.process()?.as_ref())?,
                        self.owner.parent.as_raw_handle() as usize as u64,
                    ))
                }
                pub(crate) fn resume(&self, budget: &Deadline) -> NativeResult<()> {
                    let thread = {
                        let mut state = self
                            .owner
                            .state
                            .lock()
                            .map_err(|_| NativeError::OutcomeUnknown)?;
                        if !state.valid
                            || state.call_live
                            || state.abandoned
                            || state.resume_attempted
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        budget.check()?;
                        state.resume_attempted = true;
                        state
                            .thread
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::OutcomeUnknown)?
                    };
                    let captured = self.owner.clone();
                    let initial = budget.clone();
                    let (send, receive) = std::sync::mpsc::sync_channel(1);
                    let worker = std::thread::Builder::new()
                        .name("crosspane-repair-keeper-resume".into())
                        .spawn(move || {
                            let result = (|| {
                                let proof = captured.io.admit_support(&initial)?;
                                captured.image.reverify(&captured.io, &proof, &initial)?;
                                initial.check()?;
                                // SAFETY: exact original primary thread of our validated suspended child;
                                // sticky reservation prevents a second Resume, even on a late return.
                                if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                                    return Err(NativeError::OutcomeUnknown);
                                }
                                initial.check()
                            })();
                            let _ = send.try_send(result);
                        })
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    self.owner
                        .state
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .workers
                        .push(worker);
                    match receive.recv_timeout(Duration::from_millis(budget.remaining_ms()?)) {
                        Ok(Ok(())) if budget.check().is_ok() => Ok(()),
                        _ => Err(NativeError::OutcomeUnknown),
                    }
                }
                pub(crate) fn ready(
                    &self,
                    sources: &ApprovedOuterSources,
                    budget: &Deadline,
                ) -> NativeResult<bool> {
                    let fresh = {
                        let mut state = self
                            .owner
                            .state
                            .lock()
                            .map_err(|_| NativeError::OutcomeUnknown)?;
                        let fresh = !state.ready_attempted;
                        state.ready_attempted = true;
                        fresh
                    };
                    let state = if fresh {
                        self.exchange(Method::Ready, Some(sources), budget)?
                    } else {
                        self.exchange(Method::Status, None, budget)?
                    };
                    Ok(state == State::Ready)
                }
                pub(crate) fn commit(&self, budget: &Deadline) -> NativeResult<()> {
                    {
                        let mut state = self
                            .owner
                            .state
                            .lock()
                            .map_err(|_| NativeError::OutcomeUnknown)?;
                        if state.commit_attempted {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        state.commit_attempted = true;
                    }
                    if self.exchange(Method::Commit, None, budget)? != State::Committed {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    self.owner
                        .state
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .commit_confirmed = true;
                    Ok(())
                }
                pub(crate) fn committed_here(&self) -> NativeResult<bool> {
                    Ok(self
                        .owner
                        .state
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .commit_confirmed)
                }
                pub(crate) fn status(&self, budget: &Deadline) -> NativeResult<State> {
                    self.exchange(Method::Status, None, budget)
                }
                fn exchange(
                    &self,
                    method: Method,
                    sources: Option<&ApprovedOuterSources>,
                    budget: &Deadline,
                ) -> NativeResult<State> {
                    let io = &self.owner.io;
                    let process = self.process()?;
                    let proof = io.admit_support(budget)?;
                    let nonce = io.bridge_nonce(&proof, budget)?;
                    runtime()?.block_on(async {
                        let mut pipe = loop {
                            budget.check()?;
                            match ClientOptions::new().open(endpoint(io)) {
                                Ok(pipe) => break pipe,
                                Err(error) if matches!(error.raw_os_error(), Some(2 | 231)) => {
                                    tokio::time::sleep(Duration::from_millis(5)).await
                                }
                                Err(_) => return Err(NativeError::Unavailable),
                            }
                        };
                        let peer = super::super::super::supervisor_owner::admit_repair_keeper_peer(
                            pipe.as_raw_handle(),
                            true,
                            io.clone(),
                            &io.admit_support(budget)?,
                            &process,
                            &self.owner.image,
                            self.owner.operation,
                            budget,
                        )?;
                        write_frame(
                            &mut pipe,
                            &Frame {
                                schema_version: 1,
                                operation: self.owner.operation,
                                nonce,
                                method,
                            },
                            budget,
                        )
                        .await?;
                        if method == Method::Ready {
                            let sources = sources.ok_or(NativeError::Foreign)?;
                            for role in [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl] {
                                let bytes = sources.bytes(role)?;
                                write_bounded(
                                    &mut pipe,
                                    &(bytes.len() as u64).to_le_bytes(),
                                    budget,
                                )
                                .await?;
                                for chunk in bytes.chunks(CHUNK) {
                                    write_bounded(&mut pipe, chunk, budget).await?;
                                }
                            }
                        }
                        let reply: Reply = read_frame(&mut pipe, budget).await?;
                        if reply.schema_version != 1
                            || reply.operation != self.owner.operation
                            || reply.nonce != nonce
                        {
                            return Err(NativeError::Foreign);
                        }
                        peer.reverify(io, &io.admit_support(budget)?, budget)?;
                        Ok(reply.state)
                    })
                }
            }
            pub(crate) struct RepairServer {
                io: Arc<WindowsNativeIo>,
                selection: Arc<RepairKeeperSelection>,
                namespace: Arc<RepairKeeperLease>,
                parent: Arc<RepairParent>,
                pipe: NamedPipeServer,
                rt: tokio::runtime::Runtime,
                sources: Option<Arc<ApprovedOuterSources>>,
                ready: bool,
                committed: bool,
            }
            impl RepairServer {
                pub(crate) fn new(
                    io: Arc<WindowsNativeIo>,
                    budget: &Deadline,
                ) -> NativeResult<Self> {
                    let proof = io.admit_support(budget)?;
                    let selection = Arc::new(io.select_repair_keeper(&proof, budget)?);
                    if selection.record().phase() != PayloadRepairPhase::ResumeIntent {
                        return Err(NativeError::Foreign);
                    }
                    let parent = Arc::new(RepairParent::inherited(
                        io.clone(),
                        selection.record(),
                        &proof,
                        budget,
                    )?);
                    let rt = runtime()?;
                    let (pipe, namespace) = rt.block_on(async {
                        RepairKeeperLease::reserve(
                            io.clone(),
                            &proof,
                            selection.operation(),
                            budget,
                        )
                    })?;
                    Ok(Self {
                        io,
                        selection,
                        namespace,
                        parent,
                        pipe,
                        rt,
                        sources: None,
                        ready: false,
                        committed: false,
                    })
                }
                pub(crate) fn operation(&self) -> [u8; 16] {
                    self.selection.operation()
                }
                fn receive(
                    &mut self,
                    budget: &Deadline,
                ) -> NativeResult<(
                    Frame,
                    super::super::super::supervisor_owner::RepairControlPeer,
                )> {
                    let io = self.io.clone();
                    let selection = self.selection.clone();
                    let parent = self.parent.clone();
                    self.rt.block_on(async {
                        tokio::time::timeout(
                            Duration::from_millis(budget.remaining_ms()?),
                            self.pipe.connect(),
                        )
                        .await
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .map_err(|_| NativeError::Unavailable)?;
                        let proof = io.admit_support(budget)?;
                        let peer = super::super::super::supervisor_owner::admit_repair_source_peer(
                            self.pipe.as_raw_handle(),
                            io.clone(),
                            &proof,
                            &parent,
                            io.self_image(&proof, budget)?,
                            selection.clone(),
                            selection.operation(),
                            budget,
                        )?;
                        // The exact actual inherited parent/full kernel peer/image is admitted
                        // BEFORE any claimed frame or content bytes.
                        let frame: Frame = read_frame(&mut self.pipe, budget).await?;
                        if frame.schema_version != 1
                            || frame.operation != selection.operation()
                            || frame.nonce == [0; 16]
                        {
                            return Err(NativeError::Foreign);
                        }
                        if frame.method == Method::Ready {
                            if self.sources.is_some() {
                                return Err(NativeError::OutcomeUnknown);
                            }
                            let inventory = ApprovedInventory::embedded()?;
                            let installer = ApprovedPe::own_image(selection.module())?;
                            inventory.check_staging_budget(&installer, true)?;
                            let mut values: [Vec<u8>; 3] = std::array::from_fn(|_| Vec::new());
                            for (index, role) in
                                [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl]
                                    .into_iter()
                                    .enumerate()
                            {
                                let mut prefix = [0; 8];
                                read_bounded(&mut self.pipe, &mut prefix, budget).await?;
                                let length = u64::from_le_bytes(prefix);
                                if length != inventory.role(role)?.size() {
                                    return Err(NativeError::Foreign);
                                }
                                let cap =
                                    usize::try_from(length).map_err(|_| NativeError::Oversize)?;
                                values[index]
                                    .try_reserve_exact(cap)
                                    .map_err(|_| NativeError::Oversize)?;
                                while values[index].len() < cap {
                                    let room = (cap - values[index].len()).min(CHUNK);
                                    let mut chunk = vec![0; room];
                                    read_bounded(&mut self.pipe, &mut chunk, budget).await?;
                                    values[index].extend_from_slice(&chunk);
                                    budget.check()?;
                                }
                            }
                            let sources = ApprovedOuterSources::receive(
                                values, &inventory, &installer, budget,
                            )?;
                            if sources.facts() != &selection.record().sources()[1..] {
                                return Err(NativeError::Foreign);
                            }
                            self.sources = Some(Arc::new(sources));
                        }
                        peer.reverify(&io, &io.admit_support(budget)?, budget)?;
                        Ok((frame, peer))
                    })
                }
                fn reply(
                    &mut self,
                    frame: &Frame,
                    state: State,
                    budget: &Deadline,
                ) -> NativeResult<()> {
                    let result = self.rt.block_on(write_frame(
                        &mut self.pipe,
                        &Reply {
                            schema_version: 1,
                            operation: frame.operation,
                            nonce: frame.nonce,
                            state,
                        },
                        budget,
                    ));
                    let settled = self
                        .pipe
                        .disconnect()
                        .map_err(|_| NativeError::OutcomeUnknown);
                    result?;
                    settled
                }
                pub(crate) fn await_commit(
                    &mut self,
                    budget: &Deadline,
                ) -> NativeResult<RepairCommit> {
                    loop {
                        let (frame, peer) = self.receive(budget)?;
                        match frame.method {
                            Method::Ready => {
                                if self.sources.is_none() {
                                    return Err(NativeError::Foreign);
                                }
                                super::super::super::supervisor_owner::probe_repair_support(
                                    self.io.clone(),
                                    &self.io.admit_support(budget)?,
                                    self.operation(),
                                    budget,
                                )?;
                                peer.reverify(&self.io, &self.io.admit_support(budget)?, budget)?;
                                self.ready = true;
                                self.reply(&frame, State::Ready, budget)?;
                            }
                            Method::Commit => {
                                let current = self
                                    .io
                                    .read_payload_repair(&self.io.admit_support(budget)?, budget)?
                                    .ok_or(NativeError::Missing)?;
                                if !self.ready
                                    || self.committed
                                    || current.phase() != PayloadRepairPhase::CommitIntent
                                    || !current.same_selection(self.selection.record())
                                {
                                    return Err(NativeError::Foreign);
                                }
                                self.selection.reverify(
                                    &self.io,
                                    &self.io.admit_support(budget)?,
                                    budget,
                                )?;
                                let sources =
                                    self.sources.as_ref().cloned().ok_or(NativeError::Foreign)?;
                                peer.reverify(&self.io, &self.io.admit_support(budget)?, budget)?;
                                self.committed = true;
                                let cap = RepairCommit {
                                    io: self.io.clone(),
                                    selection: self.selection.clone(),
                                    namespace: self.namespace.clone(),
                                    parent: self.parent.clone(),
                                    sources,
                                };
                                // Drop the peer's source-image read pin BEFORE giving the actual
                                // committed capability to the resident worker. ACK loss never undoes it.
                                drop(peer);
                                let _ = self.reply(&frame, State::Committed, budget);
                                return Ok(cap);
                            }
                            Method::Status => self.reply(
                                &frame,
                                if self.ready {
                                    State::Ready
                                } else {
                                    State::Retained
                                },
                                budget,
                            )?,
                        }
                    }
                }
                pub(crate) fn poll_status(
                    &mut self,
                    state: State,
                    budget: &Deadline,
                ) -> NativeResult<()> {
                    if !self.committed {
                        return Err(NativeError::Foreign);
                    }
                    let (frame, peer) = self.receive(budget)?;
                    peer.reverify(&self.io, &self.io.admit_support(budget)?, budget)?;
                    let state = if frame.method == Method::Status {
                        state
                    } else {
                        State::Retained
                    };
                    self.reply(&frame, state, budget)
                }
            }
        }
        /// Four freshly opened, independently approved fixed images. This seal can only be
        /// constructed after the actual retained predecessor tree has settled and all four
        /// positive publication observations match their reopened objects.
        #[derive(Clone)]
        pub(crate) struct RepairFixedPayload(Arc<RepairFixedData>);
        struct RepairFixedData {
            io: Arc<WindowsNativeIo>,
            record: PayloadRepairRecord,
            images: [OpenedPe; 4],
            tree: Arc<super::super::supervisor_owner::RetainedTreeCompletion>,
        }
        impl RepairFixedPayload {
            pub(crate) fn io(&self) -> &Arc<WindowsNativeIo> {
                &self.0.io
            }
            pub(crate) fn operation(&self) -> [u8; 16] {
                self.0.record.operation()
            }
            pub(crate) fn identity(&self, role: PayloadRole) -> FileIdentity {
                self.0.images[role as usize].identity()
            }
            pub(crate) fn facts(
                &self,
                role: PayloadRole,
            ) -> &super::super::super::payload::inventory::PeFacts {
                self.0.images[role as usize].approved().facts()
            }
            /// The task worker retains this one actual seal before any possibly late COM call.
            /// Cloning shares the admitted IO, original tree and opened pins; it acquires none.
            pub(crate) fn retain_for_start(&self) -> Arc<Self> {
                Arc::new(self.clone())
            }
            pub(crate) fn reverify_completion(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                budget: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.0.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                proof.check(io, budget)?;
                self.0.record.context().matches(io.target().identity())?;
                if self.0.tree.operation() != self.operation()
                    || self.0.tree.original_instance()
                        != self.0.record.original_generation().instance
                {
                    return Err(NativeError::Foreign);
                }
                self.0.tree.reverify(budget)
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                budget: &Deadline,
            ) -> NativeResult<()> {
                self.reverify_completion(io, proof, budget)?;
                let actual = io
                    .read_payload_repair(proof, budget)?
                    .ok_or(NativeError::Missing)?;
                if !actual.same_selection(&self.0.record)
                    || actual.phase().rank() < PayloadRepairPhase::FixedVerified.rank()
                    || matches!(
                        actual.phase(),
                        PayloadRepairPhase::Cancelled
                            | PayloadRepairPhase::Unknown
                            | PayloadRepairPhase::Retired
                    )
                {
                    return Err(NativeError::Foreign);
                }
                for role in PayloadRole::ALL {
                    let pin = &self.0.images[role as usize];
                    pin.reverify(io, proof, budget)?;
                    let observed = actual.role(role).published().ok_or(NativeError::Foreign)?;
                    if observed.identity != FileStamp::from(pin.identity())
                        || observed.facts != *pin.approved().facts()
                    {
                        return Err(NativeError::Foreign);
                    }
                }
                budget.check()
            }
        }
        /// Private actual selection. No metadata constructor can manufacture this original IO,
        /// own opened module, or actual original agent observation.
        pub(crate) struct NativePayloadRepairSelection {
            io: Arc<WindowsNativeIo>,
            module: SelfImagePin,
            original: Mutex<Option<AgentObservation>>,
            selected: PayloadRepairSelection,
            keeper: Option<RepairKeeperSelection>,
        }
        impl NativePayloadRepairSelection {
            pub(crate) fn metadata(&self) -> &PayloadRepairSelection {
                &self.selected
            }
            pub(crate) fn module(&self) -> &SelfImagePin {
                &self.module
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                budget: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                io.verify_stop_lock(proof, lock, budget)?;
                self.selected.context().matches(io.target().identity())?;
                self.module.reverify(io, proof, budget)?;
                if let Some(keeper) = &self.keeper {
                    keeper.reverify(io, proof, budget)?;
                    if keeper.record().selection() != &self.selected {
                        return Err(NativeError::Foreign);
                    }
                    return budget.check();
                }
                let own = io.own_process_identity(proof, budget)?;
                let original = self.selected.source_process();
                if own.pid() != original.pid()
                    || own.creation() != original.creation()
                    || self.module.identity()
                        != (FileIdentity {
                            volume: self.selected.own_module().identity.volume,
                            file: self.selected.own_module().identity.file,
                        })
                    || self.module.facts() != &self.selected.own_module().facts
                {
                    return Err(NativeError::Foreign);
                }
                // The source retains the genuine original observation until handoff. Metadata
                // cannot replace that observation or select another process after an error.
                let held = self
                    .original
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                let agent = held.as_ref().ok_or(NativeError::Foreign)?;
                agent.revalidate(io, proof, budget)?;
                if io.agent_generation(agent, proof, budget)? != self.selected.original_generation()
                {
                    return Err(NativeError::Foreign);
                }
                budget.check()
            }
        }
        /// Correlated durable bytes; callers still need the actual original lock and support
        /// proof at every effect. It deliberately holds no cloned lock across task handoff.
        pub(crate) struct RepairMutationPermit {
            io: Arc<WindowsNativeIo>,
            bytes: Vec<u8>,
        }
        impl RepairMutationPermit {
            pub(crate) fn document(&self) -> NativeResult<PayloadRepairRecord> {
                PayloadRepairRecord::decode(&self.bytes)
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                budget: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                io.verify_stop_lock(proof, lock, budget)?;
                let context = io.context.clone();
                let lease = lock.0.clone();
                let expected = self.bytes.clone();
                let actual_budget = proof.budget(io, budget)?;
                io.owner.run(Dispatch::Observation, budget, move || {
                    validate_payload_lock(&context, &lease, &actual_budget)?;
                    clean_publication(&lease.parent, &context, &actual_budget)?;
                    let (_, actual) = read(
                        &lease.parent,
                        &context,
                        records::RecordName::RepairPayload,
                        &actual_budget,
                    )?
                    .ok_or(NativeError::Foreign)?;
                    if actual != expected {
                        return Err(NativeError::Foreign);
                    }
                    let record = PayloadRepairRecord::decode(&actual)?;
                    record.context().matches(&context.target.identity)?;
                    check_slot(&lease.parent, &context, &record, &actual_budget)
                })
            }
        }
        impl WindowsNativeIo {
            pub(crate) fn select_payload_repair(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                observed: &RepairObservation,
                budget: &Deadline,
            ) -> NativeResult<Arc<NativePayloadRepairSelection>> {
                self.verify_stop_lock(proof, lock, budget)?;
                self.reject_active_first_install(proof, budget)?;
                let actual = self.observe_payload_repair(proof, budget)?;
                if actual != *observed
                    || controller::classify(&actual)? != PayloadRepairDecision::Eligible
                {
                    return Err(NativeError::Unsupported);
                }
                let context = self.context.clone();
                let lease = lock.0.clone();
                let checked = proof.budget(self, budget)?;
                self.owner.run(Dispatch::Observation, budget, move || {
                    refuse_old_operations(&context, &lease, &checked)?;
                    refuse_active(&context, &lease, &checked)
                })?;
                let module = self.self_image(&self.admit_support(budget)?, budget)?;
                let own = self.own_process_identity(&self.admit_support(budget)?, budget)?;
                let original = self.observe_agent(&self.admit_support(budget)?, budget)?;
                if original.bootstrap().phase != crate::agent_contract::BootstrapPhase::Ready {
                    return Err(NativeError::Unsupported);
                }
                let generation =
                    self.agent_generation(&original, &self.admit_support(budget)?, budget)?;
                let root = self.payload_root(&self.admit_support(budget)?, lock, budget)?;
                let mut fixed = [OriginalLeaf::Missing; 4];
                for (index, role) in PayloadRole::ALL.into_iter().enumerate() {
                    fixed[index] = root
                        .observe_opaque(self, &self.admit_support(budget)?, role, budget)?
                        .map(|leaf| OriginalLeaf::Present(leaf.identity().into()))
                        .unwrap_or(OriginalLeaf::Missing);
                }
                let inventory = ApprovedInventory::embedded()?;
                let own_expected = ApprovedPe::own_image(&module)?;
                let sources = [
                    own_expected.facts().clone(),
                    inventory.role(PayloadRole::Agent)?.facts().clone(),
                    inventory.role(PayloadRole::Ui)?.facts().clone(),
                    inventory.role(PayloadRole::Ctl)?.facts().clone(),
                ];
                let metadata = PayloadRepairSelection::new(
                    super::super::super::payload::recovery::OuterContextCorrelation::new(
                        self.target().identity(),
                    )?,
                    PayloadRepairProcess::new(own.pid(), own.creation())?,
                    ImageObservation {
                        identity: module.identity().into(),
                        facts: module.facts().clone(),
                    },
                    generation,
                    original.bootstrap().started_unix_ms,
                    PayloadRepairTask::new(
                        actual.task().diagnostic(),
                        actual
                            .task()
                            .expected_xml()
                            .ok_or(NativeError::Unsupported)?
                            .to_owned(),
                    )?,
                    sources,
                    fixed,
                )?;
                let result = Arc::new(NativePayloadRepairSelection {
                    io: self.clone(),
                    module,
                    original: Mutex::new(Some(original)),
                    selected: metadata,
                    keeper: None,
                });
                result.reverify(self, &self.admit_support(budget)?, lock, budget)?;
                Ok(result)
            }
            pub(crate) fn admitted_keeper_repair_selection(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                budget: &Deadline,
            ) -> NativeResult<Arc<NativePayloadRepairSelection>> {
                self.verify_stop_lock(proof, lock, budget)?;
                let keeper = self.select_repair_keeper(proof, budget)?;
                let selected = keeper.record().selection().clone();
                let module = self.self_image(proof, budget)?;
                let result = Arc::new(NativePayloadRepairSelection {
                    io: self.clone(),
                    module,
                    original: Mutex::new(None),
                    selected,
                    keeper: Some(keeper),
                });
                result.reverify(self, proof, lock, budget)?;
                Ok(result)
            }
            pub(crate) fn refuse_unsettled_payload_repair_readonly(
                &self,
                proof: &SupportProof,
                budget: &Deadline,
            ) -> NativeResult<()> {
                let context = self.context.clone();
                let checked = proof.budget(self, budget)?;
                self.owner.run(Dispatch::Observation, budget, move || {
                    context.validate(&checked)?;
                    let Some(parent) = Anchor::open(
                        context.target.paths.installer(),
                        &context.security,
                        true,
                        &checked,
                    )?
                    else {
                        return Ok(());
                    };
                    clean_publication(&parent, &context, &checked)?;
                    let history = catalog(&parent, &context, &checked)?;
                    if (0..3)
                        .filter_map(|slot| history.get(slot))
                        .any(|slot| slot.phase() == PayloadRepairCatalogPhase::Active)
                    {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    if let Some((_, bytes)) = read(
                        &parent,
                        &context,
                        records::RecordName::RepairPayload,
                        &checked,
                    )? {
                        let record = PayloadRepairRecord::decode(&bytes)?;
                        record.context().same_user(&context.target.identity)?;
                        if !matches!(
                            record.phase(),
                            PayloadRepairPhase::Complete
                                | PayloadRepairPhase::Retired
                                | PayloadRepairPhase::Cancelled
                        ) {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        check_slot(&parent, &context, &record, &checked)?;
                    }
                    checked.check()
                })
            }
            pub(crate) fn refuse_unsettled_payload_repair(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                budget: &Deadline,
            ) -> NativeResult<()> {
                self.verify_stop_lock(proof, lock, budget)?;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let checked = proof.budget(self, budget)?;
                self.owner.run(Dispatch::Observation, budget, move || {
                    refuse_active(&context, &lease, &checked)
                })
            }
            /// Source-only terminal path. The actual create worker is joined and any child
            /// is positively exited without Resume before this seal reaches file cleanup.
            pub(crate) fn cancel_payload_repair(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                selected: &Arc<NativePayloadRepairSelection>,
                settled: Arc<keeper::SourceCancellation>,
                budget: &Deadline,
            ) -> NativeResult<PayloadRepairRecord> {
                selected.reverify(self, proof, lock, budget)?;
                if selected.keeper.is_some() {
                    return Err(NativeError::Foreign);
                }
                let context = self.context.clone();
                let lease = lock.0.clone();
                let checked = proof.budget(self, budget)?;
                let metadata = selected.metadata().clone();
                let io = self.clone();
                self.owner.run(Dispatch::Mutation, budget, move || {
                    let change = Change::new();
                    change.finish((|| {
                        refuse_old_operations(&context, &lease, &checked)?;
                        let (_, raw) = read(
                            &lease.parent,
                            &context,
                            records::RecordName::RepairPayload,
                            &checked,
                        )?
                        .ok_or(NativeError::Missing)?;
                        let current = PayloadRepairRecord::decode(&raw)?;
                        current.context().matches(&context.target.identity)?;
                        if current.selection() != &metadata
                            || !(source_can_cancel(current.phase(), false)
                                || current.phase() == PayloadRepairPhase::Cancelled)
                        {
                            return Err(NativeError::Foreign);
                        }
                        settled.reverify(io.as_ref(), current.operation(), &checked)?;
                        check_slot(&lease.parent, &context, &current, &checked)?;
                        let mut cancelled = current.clone();
                        cancelled.advance(PayloadRepairPhase::Cancelled)?;
                        let bytes = cancelled.encode()?;
                        let (mut publication, state) =
                            publication(&lease.parent, &context, &checked)?;
                        if current.phase() != PayloadRepairPhase::Cancelled {
                            if !terminal_publication_admitted(
                                publication.as_ref().map(|(_, _, intent)| intent),
                                state,
                                current.operation(),
                                Target::Record,
                                &bytes,
                            ) {
                                return Err(NativeError::OutcomeUnknown);
                            }
                            if !publication.as_ref().is_some_and(|(_, _, intent)| {
                                intent.matches_request(current.operation(), Target::Record, &bytes)
                            }) {
                                // Re-read the exact source stamp; never construct an identity from facts.
                                let prior = read(
                                    &lease.parent,
                                    &context,
                                    records::RecordName::RepairPayload,
                                    &checked,
                                )?;
                                let intent = Intent::new(
                                    current.operation(),
                                    Target::Record,
                                    observed(&prior)?,
                                    &bytes,
                                )?;
                                persist_intent(
                                    &lease.parent,
                                    &context,
                                    &mut publication,
                                    intent,
                                    &checked,
                                    &change,
                                )?;
                            }
                        }
                        if let Some(parent) = repair_copy_parent(
                            &context,
                            current.operation(),
                            false,
                            &checked,
                            &change,
                        )? {
                            if let Some(id) = parent.removal_copy_identity(
                                "keeper-copy.exe",
                                &context.security,
                                &checked,
                            )? {
                                if FileStamp::from(id) == current.own_module().identity {
                                    return Err(NativeError::Foreign);
                                }
                                if let Some(image) = current.keeper().image()
                                    && image.identity != FileStamp::from(id)
                                {
                                    return Err(NativeError::Foreign);
                                }
                                settled.reverify(io.as_ref(), current.operation(), &checked)?;
                                change.reached();
                                parent.delete_removal_copy(
                                    "keeper-copy.exe",
                                    id,
                                    &context.security,
                                    &checked,
                                )?;
                            }
                            if parent
                                .removal_copy_identity(
                                    "keeper-copy.exe",
                                    &context.security,
                                    &checked,
                                )?
                                .is_some()
                            {
                                return Err(NativeError::OutcomeUnknown);
                            }
                        }
                        settled.reverify(io.as_ref(), current.operation(), &checked)?;
                        if current.phase() != PayloadRepairPhase::Cancelled
                            || publication.as_ref().is_some_and(|(_, _, intent)| {
                                intent.matches_request(current.operation(), Target::Record, &bytes)
                            })
                        {
                            publish_fixed(
                                &context,
                                &lease,
                                current.operation(),
                                Target::Record,
                                &bytes,
                                &checked,
                                &change,
                            )?;
                        }
                        // Slot release follows durable Cancelled plus positive child/copy absence.
                        let mut history = catalog(&lease.parent, &context, &checked)?;
                        history.release_cancelled(&cancelled)?;
                        publish_fixed(
                            &context,
                            &lease,
                            current.operation(),
                            Target::Catalog,
                            &history.encode()?,
                            &checked,
                            &change,
                        )?;
                        Ok(cancelled)
                    })())
                })
            }
            pub(crate) fn retire_settled_repair_copy(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                budget: &Deadline,
            ) -> NativeResult<()> {
                self.verify_stop_lock(proof, lock, budget)?;
                let Some(record) = self.read_payload_repair(proof, budget)? else {
                    return Ok(());
                };
                if record.phase() == PayloadRepairPhase::Cancelled {
                    return Ok(());
                }
                if !matches!(
                    record.phase(),
                    PayloadRepairPhase::Complete | PayloadRepairPhase::Retired
                ) {
                    return Err(NativeError::OutcomeUnknown);
                }
                record.context().same_user(self.target().identity())?;
                if record.phase() == PayloadRepairPhase::Retired {
                    let context = self.context.clone();
                    let lease = lock.0.clone();
                    let checked = proof.budget(self, budget)?;
                    let expected_record = record.encode()?;
                    let settled = self.owner.run(Dispatch::Observation, budget, move || {
                        check_terminal_repair_history(&context, &lease, &expected_record, &checked)
                            .map(|(_, settled)| settled)
                    })?;
                    if settled {
                        // No namespace reservation, copy open, DELETE or publication follows.
                        return Ok(());
                    }
                }
                let copy = record.keeper().image().ok_or(NativeError::Foreign)?;
                let expected = FileIdentity {
                    volume: copy.identity.volume,
                    file: copy.identity.file,
                };
                let own = self.self_image(proof, budget)?;
                if own.identity() == expected {
                    return Err(NativeError::Foreign);
                }
                // An actual kernel-exclusive repair namespace, never terminal record facts,
                // proves no live keeper owns this user/session. It grants FILE cleanup only.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| NativeError::Unavailable)?;
                let (_pipe, namespace) = runtime.block_on(async {
                    keeper::RepairKeeperLease::reserve(
                        self.clone(),
                        proof,
                        record.operation(),
                        budget,
                    )
                })?;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let checked = proof.budget(self, budget)?;
                let io = self.clone();
                let owner = self.owner.clone();
                let expected_record = record.encode()?;
                // Admit outside the owner callback: dispatching recursively on the same owner
                // would deadlock. The proof and genuine namespace are renewed in the callback.
                let namespace_proof = self.admit_support(budget)?;
                self.owner.run(Dispatch::Mutation, budget, move || {
                    let change = Change::new();
                    let result = change.finish((|| {
                        let (current, settled) = check_terminal_repair_history(
                            &context,
                            &lease,
                            &expected_record,
                            &checked,
                        )?;
                        if settled {
                            return Ok(());
                        }
                        let mut retired = current.clone();
                        retired.advance(PayloadRepairPhase::Retired)?;
                        let bytes = retired.encode()?;
                        namespace.reverify_on_owner(
                            &io,
                            &namespace_proof,
                            current.operation(),
                            &checked,
                        )?;
                        let parent = repair_copy_parent(
                            &context,
                            current.operation(),
                            false,
                            &checked,
                            &change,
                        )?
                        .ok_or(NativeError::Missing)?;
                        let name = records::RecordName::RepairPayload;
                        let prior = read(&lease.parent, &context, name, &checked)?;
                        let (mut publication, state) =
                            publication(&lease.parent, &context, &checked)?;
                        if current.phase() != PayloadRepairPhase::Retired
                            && !terminal_publication_admitted(
                                publication.as_ref().map(|(_, _, intent)| intent),
                                state,
                                current.operation(),
                                Target::Record,
                                &bytes,
                            )
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        // Durable existing publication-intent protocol records the next Retired
                        // bytes BEFORE DELETE. Retired itself publishes only after positive absence.
                        let intent = Intent::new(
                            current.operation(),
                            Target::Record,
                            observed(&prior)?,
                            &bytes,
                        )?;
                        if current.phase() != PayloadRepairPhase::Retired
                            && !publication.as_ref().is_some_and(|(_, _, old)| {
                                old.matches_request(current.operation(), Target::Record, &bytes)
                            })
                        {
                            persist_intent(
                                &lease.parent,
                                &context,
                                &mut publication,
                                intent,
                                &checked,
                                &change,
                            )?;
                        }
                        namespace.reverify_on_owner(
                            &io,
                            &namespace_proof,
                            current.operation(),
                            &checked,
                        )?;
                        if let Some(actual) = parent.removal_copy_identity(
                            "keeper-copy.exe",
                            &context.security,
                            &checked,
                        )? {
                            if actual != expected {
                                return Err(NativeError::Foreign);
                            }
                            change.reached();
                            // Existing exact regular-copy primitive: same-handle exclusive DELETE,
                            // no POSIX/ADS/reboot trick, no executing-image bypass, positive absence.
                            parent.delete_removal_copy(
                                "keeper-copy.exe",
                                expected,
                                &context.security,
                                &checked,
                            )?;
                        }
                        if parent
                            .removal_copy_identity("keeper-copy.exe", &context.security, &checked)?
                            .is_some()
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        namespace.reverify_on_owner(
                            &io,
                            &namespace_proof,
                            current.operation(),
                            &checked,
                        )?;
                        if current.phase() != PayloadRepairPhase::Retired
                            || publication.as_ref().is_some_and(|(_, _, intent)| {
                                intent.matches_request(current.operation(), Target::Record, &bytes)
                            })
                        {
                            publish_fixed(
                                &context,
                                &lease,
                                current.operation(),
                                Target::Record,
                                &bytes,
                                &checked,
                                &change,
                            )?;
                        }
                        let mut history = catalog(&lease.parent, &context, &checked)?;
                        history.retire(retired.slot().ok_or(NativeError::Foreign)?, &retired)?;
                        publish_fixed(
                            &context,
                            &lease,
                            current.operation(),
                            Target::Catalog,
                            &history.encode()?,
                            &checked,
                            &change,
                        )
                    })());
                    if matches!(result, Err(NativeError::OutcomeUnknown)) {
                        owner.retire_mutations();
                    }
                    result
                })
            }
            pub(crate) fn reserve_payload_repair(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                selected: &Arc<NativePayloadRepairSelection>,
                operation: [u8; 16],
                budget: &Deadline,
            ) -> NativeResult<PayloadRepairRecord> {
                selected.reverify(self, proof, lock, budget)?;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let checked = proof.budget(self, budget)?;
                let metadata = selected.metadata().clone();
                let owner = self.owner.clone();
                self.owner.run(Dispatch::Mutation, budget, move || {
                    let change = Change::new();
                    let result = change.finish((|| {
                        refuse_active(&context, &lease, &checked)?;
                        let mut record = PayloadRepairRecord::new(operation, metadata, None)?;
                        let mut history = catalog(&lease.parent, &context, &checked)?;
                        let slot = history.reserve(&record)?;
                        record.bind_slot(slot)?;
                        // Preserve the complete previous record in its fixed operation directory
                        // before replacing the selection pointer. This copy grants no authority.
                        if let Some((_, old)) = read(
                            &lease.parent,
                            &context,
                            records::RecordName::RepairPayload,
                            &checked,
                        )? {
                            let prior = PayloadRepairRecord::decode(&old)?;
                            check_slot(&lease.parent, &context, &prior, &checked)?;
                            let parent = repair_copy_parent(
                                &context,
                                prior.operation(),
                                prior.phase() == PayloadRepairPhase::Cancelled,
                                &checked,
                                &change,
                            )?
                            .ok_or(NativeError::Missing)?;
                            let name = records::RecordName::RepairPayload.file_name()?;
                            match parent.read_private(
                                &name,
                                &context.security,
                                files::MAX_RECORD_BYTES,
                                &checked,
                            )? {
                                Some((_, bytes)) if bytes == old => {}
                                Some(_) => return Err(NativeError::Foreign),
                                None => {
                                    change.reached();
                                    let mut file = parent.create_private(
                                        &name,
                                        &context.security,
                                        &checked,
                                    )?;
                                    use std::io::Write;
                                    file.write_all(&old)
                                        .map_err(|_| NativeError::OutcomeUnknown)?;
                                    file.sync_all().map_err(|_| NativeError::OutcomeUnknown)?;
                                    checked.check()?;
                                    let facts =
                                        native::observe(&file, name.as_str(), &context.security)?;
                                    files::admit_component(&facts, Admission::PrivateFile)?;
                                    let id = facts.identity;
                                    drop(file);
                                    let seen = parent
                                        .read_private(
                                            &name,
                                            &context.security,
                                            files::MAX_RECORD_BYTES,
                                            &checked,
                                        )?
                                        .ok_or(NativeError::OutcomeUnknown)?;
                                    if seen != (id, old) {
                                        return Err(NativeError::OutcomeUnknown);
                                    }
                                }
                            }
                        }
                        publish_fixed(
                            &context,
                            &lease,
                            operation,
                            Target::Catalog,
                            &history.encode()?,
                            &checked,
                            &change,
                        )?;
                        publish_fixed(
                            &context,
                            &lease,
                            operation,
                            Target::Record,
                            &record.encode()?,
                            &checked,
                            &change,
                        )?;
                        Ok(record)
                    })());
                    if matches!(result, Err(NativeError::OutcomeUnknown)) {
                        owner.retire_mutations();
                    }
                    result
                })
            }
            pub(crate) fn admit_payload_repair_permit(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                budget: &Deadline,
            ) -> NativeResult<RepairMutationPermit> {
                self.verify_stop_lock(proof, lock, budget)?;
                let record = self
                    .read_payload_repair(proof, budget)?
                    .ok_or(NativeError::Missing)?;
                let permit = RepairMutationPermit {
                    io: self.clone(),
                    bytes: record.encode()?,
                };
                permit.reverify(self, proof, lock, budget)?;
                Ok(permit)
            }
            pub(crate) fn prepare_repair_copy(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RepairMutationPermit,
                own: &SelfImagePin,
                budget: &Deadline,
            ) -> NativeResult<OpenedPe> {
                permit.reverify(self, proof, lock, budget)?;
                own.reverify(self, proof, budget)?;
                let record = permit.document()?;
                if record.phase() != PayloadRepairPhase::CopyIntent
                    || record.keeper().image().is_some()
                    || own.facts() != &record.own_module().facts
                    || FileStamp::from(own.identity()) != record.own_module().identity
                {
                    return Err(NativeError::Foreign);
                }
                let expected = ApprovedPe::own_image(own)?;
                let input = self.self_image_reader(own, proof, budget)?;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let checked = proof.budget(self, budget)?;
                self.owner.run(Dispatch::Mutation, budget, move || {
                    let change = Change::new();
                    change.finish((|| {
                        let record = check_repair_record(&context, &lease, &bytes, &checked)?;
                        let parent = repair_copy_parent(
                            &context,
                            record.operation(),
                            true,
                            &checked,
                            &change,
                        )?
                        .ok_or(NativeError::Missing)?;
                        change.reached();
                        let image = parent.stage_image(
                            "keeper-copy.exe",
                            input,
                            &expected,
                            &context.security,
                            &checked,
                        )?;
                        Ok(OpenedPe(Arc::new(ApprovedImage {
                            target: context.target.nonce,
                            parent,
                            leaf: "keeper-copy.exe".into(),
                            image,
                            expected,
                        })))
                    })())
                })
            }
            #[allow(clippy::too_many_arguments)] // Genuine separate lock, permit, tree and pin gates.
            pub(crate) fn stage_repair(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RepairMutationPermit,
                tree: Arc<super::super::supervisor_owner::RetainedTreeCompletion>,
                role: PayloadRole,
                input: Box<dyn std::io::Read + Send>,
                expected: &ApprovedPe,
                budget: &Deadline,
            ) -> NativeResult<StagedPe> {
                permit.reverify(self, proof, lock, budget)?;
                let record = permit.document()?;
                native_tree(&record, &tree, budget)?;
                if record.phase() != (PayloadRepairPhase::StageIntent { role })
                    || expected.role() != role
                    || expected.facts() != record.selection().source(role)
                {
                    return Err(NativeError::Foreign);
                }
                let root = self.payload_root(proof, lock, budget)?.0;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let checked = proof.budget(self, budget)?;
                let expected = expected.clone();
                self.owner.run(Dispatch::Mutation, budget, move || {
                    let change = Change::new();
                    change.finish((|| {
                        let record = check_repair_record(&context, &lease, &bytes, &checked)?;
                        native_tree(&record, &tree, &checked)?;
                        let root = PayloadRoot(root)
                            .check(&context, &checked)?
                            .ok_or(NativeError::Missing)?;
                        let stages = ensure_payload_child(
                            &root,
                            "repair-stage",
                            &context,
                            &checked,
                            &change,
                        )?;
                        let parent = Arc::new(ensure_payload_child(
                            &stages,
                            &records::hex(&record.operation()),
                            &context,
                            &checked,
                            &change,
                        )?);
                        change.reached();
                        let image = parent.stage_image(
                            role.leaf(),
                            input,
                            &expected,
                            &context.security,
                            &checked,
                        )?;
                        native_tree(&record, &tree, &checked)?;
                        Ok(StagedPe {
                            target: context.target.nonce,
                            operation: record.operation(),
                            role,
                            parent,
                            image,
                            expected,
                        })
                    })())
                })
            }
            pub(crate) fn backup_repair(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RepairMutationPermit,
                tree: Arc<super::super::supervisor_owner::RetainedTreeCompletion>,
                role: PayloadRole,
                budget: &Deadline,
            ) -> NativeResult<Option<FileStamp>> {
                permit.reverify(self, proof, lock, budget)?;
                let record = permit.document()?;
                native_tree(&record, &tree, budget)?;
                if record.phase() != (PayloadRepairPhase::BackupIntent { role }) {
                    return Err(NativeError::Foreign);
                }
                let root = self.payload_root(proof, lock, budget)?.0;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let checked = proof.budget(self, budget)?;
                self.owner.run(Dispatch::Mutation, budget, move || {
                    let change = Change::new();
                    change.finish((|| {
                        let record = check_repair_record(&context, &lease, &bytes, &checked)?;
                        native_tree(&record, &tree, &checked)?;
                        let root = PayloadRoot(root)
                            .check(&context, &checked)?
                            .ok_or(NativeError::Missing)?;
                        let observed =
                            root.opaque(role.leaf(), true, &context.security, &checked)?;
                        match (record.role(role).original(), observed) {
                            (Some(OriginalLeaf::Missing), None) => Ok(None),
                            (Some(OriginalLeaf::Present(id)), Some(observed))
                                if FileStamp::from(observed.identity) == id =>
                            {
                                let backups = ensure_payload_child(
                                    &root,
                                    "repair-backups",
                                    &context,
                                    &checked,
                                    &change,
                                )?;
                                let parent = ensure_payload_child(
                                    &backups,
                                    &records::hex(&record.operation()),
                                    &context,
                                    &checked,
                                    &change,
                                )?;
                                native_tree(&record, &tree, &checked)?;
                                change.reached();
                                let moved = root.move_opaque(
                                    observed,
                                    &parent,
                                    role.leaf(),
                                    &context.security,
                                    &checked,
                                )?;
                                let seen = parent
                                    .opaque(role.leaf(), false, &context.security, &checked)?
                                    .ok_or(NativeError::OutcomeUnknown)?;
                                if seen.identity != moved
                                    || root
                                        .opaque(role.leaf(), false, &context.security, &checked)?
                                        .is_some()
                                {
                                    return Err(NativeError::OutcomeUnknown);
                                }
                                Ok(Some(moved.into()))
                            }
                            _ => Err(NativeError::Foreign),
                        }
                    })())
                })
            }
            pub(crate) fn publish_repair(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RepairMutationPermit,
                tree: Arc<super::super::supervisor_owner::RetainedTreeCompletion>,
                staged: StagedPe,
                budget: &Deadline,
            ) -> NativeResult<OpenedPe> {
                permit.reverify(self, proof, lock, budget)?;
                let record = permit.document()?;
                native_tree(&record, &tree, budget)?;
                if record.phase() != (PayloadRepairPhase::PublishIntent { role: staged.role })
                    || staged.target != self.context.target.nonce
                    || staged.operation != record.operation()
                    || record.role(staged.role).staged()
                        != Some(&ImageObservation {
                            identity: staged.image.identity.into(),
                            facts: staged.image.facts.clone(),
                        })
                {
                    return Err(NativeError::Foreign);
                }
                let root = self.payload_root(proof, lock, budget)?.0;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let checked = proof.budget(self, budget)?;
                self.owner.run(Dispatch::Mutation, budget, move || {
                    let change = Change::new();
                    change.finish((|| {
                        let record = check_repair_record(&context, &lease, &bytes, &checked)?;
                        native_tree(&record, &tree, &checked)?;
                        let root = PayloadRoot(root)
                            .check(&context, &checked)?
                            .ok_or(NativeError::Missing)?;
                        let fresh = native::measure_image(
                            staged.image.file.clone(),
                            staged.expected.version(),
                            &checked,
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
                            &checked,
                        )?;
                        let id = staged.image.identity;
                        let expected = staged.expected;
                        let role = staged.role;
                        drop(staged.image);
                        let image = root.open_image(
                            role.leaf(),
                            true,
                            expected.version(),
                            &context.security,
                            &checked,
                        )?;
                        if image.identity != id
                            || image.facts != *expected.facts()
                            || staged
                                .parent
                                .opaque(role.leaf(), false, &context.security, &checked)?
                                .is_some()
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        native_tree(&record, &tree, &checked)?;
                        Ok(OpenedPe(Arc::new(ApprovedImage {
                            target: context.target.nonce,
                            parent: root,
                            leaf: role.leaf().into(),
                            image,
                            expected,
                        })))
                    })())
                })
            }
            fn pin_repair_copy(
                self: &Arc<Self>,
                proof: &SupportProof,
                record: &PayloadRepairRecord,
                budget: &Deadline,
            ) -> NativeResult<RepairPeerImage> {
                record.context().matches(self.target().identity())?;
                let expected = record.keeper().image().ok_or(NativeError::Missing)?.clone();
                let context = self.context.clone();
                let original = record.clone();
                let io = self.clone();
                let checked = proof.budget(self, budget)?;
                self.owner.run(Dispatch::Observation, budget, move || {
                    context.validate(&checked)?;
                    let parent = repair_copy_parent(
                        &context,
                        original.operation(),
                        false,
                        &checked,
                        &Change::new(),
                    )?
                    .ok_or(NativeError::Missing)?;
                    let image = parent.open_image(
                        "keeper-copy.exe",
                        true,
                        &expected.facts.version,
                        &context.security,
                        &checked,
                    )?;
                    if FileStamp::from(image.identity) != expected.identity
                        || image.facts != expected.facts
                    {
                        return Err(NativeError::Foreign);
                    }
                    Ok(RepairPeerImage(Arc::new(RepairPeerData {
                        io,
                        record: original,
                        parent,
                        image,
                    })))
                })
            }
            pub(crate) fn select_repair_keeper(
                self: &Arc<Self>,
                proof: &SupportProof,
                budget: &Deadline,
            ) -> NativeResult<RepairKeeperSelection> {
                let record = self
                    .read_payload_repair(proof, budget)?
                    .ok_or(NativeError::Missing)?;
                if matches!(
                    record.phase(),
                    PayloadRepairPhase::Selected
                        | PayloadRepairPhase::CopyIntent
                        | PayloadRepairPhase::CopyReady
                        | PayloadRepairPhase::HandoffIntent
                        | PayloadRepairPhase::Cancelled
                        | PayloadRepairPhase::Unknown
                        | PayloadRepairPhase::Retired
                ) {
                    return Err(NativeError::Foreign);
                }
                let module = self.self_image(proof, budget)?;
                let own = self.own_process_identity(proof, budget)?;
                let image = self.pin_repair_copy(proof, &record, budget)?;
                let selected = RepairKeeperSelection {
                    io: self.clone(),
                    module,
                    own,
                    image,
                    record,
                };
                selected.reverify(self, proof, budget)?;
                Ok(selected)
            }
            pub(crate) fn prepare_repair_completion(
                self: &Arc<Self>,
                proof: &SupportProof,
                lease: &StopLockLease,
                operation: [u8; 16],
                budget: &Deadline,
            ) -> NativeResult<RepairCompletionAdmission> {
                if !std::ptr::eq(self.as_ref(), lease.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                lease.reverify(proof, budget)?;
                let selection = self.select_repair_keeper(proof, budget)?;
                if selection.operation() != operation {
                    return Err(NativeError::Foreign);
                }
                let admitted = RepairCompletionAdmission { selection };
                admitted.reverify(self, proof, budget)?;
                Ok(admitted)
            }
            pub(crate) fn pin_repair_peer(
                self: &Arc<Self>,
                proof: &SupportProof,
                operation: [u8; 16],
                peer: &super::super::supervisor_owner::KernelOuterPeer,
                budget: &Deadline,
            ) -> NativeResult<RepairPeerImage> {
                peer.reverify(self, proof, budget)?;
                let record = self
                    .read_payload_repair(proof, budget)?
                    .ok_or(NativeError::Missing)?;
                if record.operation() != operation
                    || record.keeper().child()
                        != Some(PayloadRepairProcess::new(peer.pid(), peer.creation())?)
                {
                    return Err(NativeError::Foreign);
                }
                record.context().matches(peer.token())?;
                let image = self.pin_repair_copy(proof, &record, budget)?;
                if process::literal_path(peer.image())?
                    != process::literal_path(image.canonical_dos_path())?
                {
                    return Err(NativeError::Foreign);
                }
                peer.reverify(self, proof, budget)?;
                image.reverify(self, proof, budget)?;
                Ok(image)
            }
            pub(crate) fn verify_repaired_payload(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RepairMutationPermit,
                tree: Arc<super::super::supervisor_owner::RetainedTreeCompletion>,
                budget: &Deadline,
            ) -> NativeResult<RepairFixedPayload> {
                permit.reverify(self, proof, lock, budget)?;
                let record = permit.document()?;
                if record.phase()
                    != (PayloadRepairPhase::Published {
                        role: PayloadRole::Ctl,
                    })
                    || tree.operation() != record.operation()
                    || tree.original_instance() != record.original_generation().instance
                {
                    return Err(NativeError::Foreign);
                }
                tree.reverify(budget)?;
                let root = self.payload_root(proof, lock, budget)?;
                // Current record facts only correlate. Approval comes again from this
                // executing build and its genuinely opened own module, never from the record.
                let module = self.self_image(proof, budget)?;
                let installer = ApprovedPe::own_image(&module)?;
                let inventory = ApprovedInventory::embedded()?;
                if installer.facts() != record.selection().source(PayloadRole::Installer) {
                    return Err(NativeError::Foreign);
                }
                for role in [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl] {
                    if inventory.role(role)?.facts() != record.selection().source(role) {
                        return Err(NativeError::Foreign);
                    }
                }
                let images = [
                    root.open_approved(self, proof, PayloadRole::Installer, &installer, budget)?,
                    root.open_approved(
                        self,
                        proof,
                        PayloadRole::Agent,
                        inventory.role(PayloadRole::Agent)?,
                        budget,
                    )?,
                    root.open_approved(
                        self,
                        proof,
                        PayloadRole::Ui,
                        inventory.role(PayloadRole::Ui)?,
                        budget,
                    )?,
                    root.open_approved(
                        self,
                        proof,
                        PayloadRole::Ctl,
                        inventory.role(PayloadRole::Ctl)?,
                        budget,
                    )?,
                ];
                for role in PayloadRole::ALL {
                    let image = &images[role as usize];
                    let observed = record.role(role).published().ok_or(NativeError::Foreign)?;
                    if observed.identity != image.identity().into()
                        || observed.facts != *image.approved().facts()
                    {
                        return Err(NativeError::Foreign);
                    }
                }
                permit.reverify(self, proof, lock, budget)?;
                tree.reverify(budget)?;
                Ok(RepairFixedPayload(Arc::new(RepairFixedData {
                    io: self.clone(),
                    record,
                    images,
                    tree,
                })))
            }
            pub(crate) fn read_payload_repair(
                &self,
                proof: &SupportProof,
                budget: &Deadline,
            ) -> NativeResult<Option<PayloadRepairRecord>> {
                self.read_record(
                    proof,
                    records::RecordName::RepairPayload,
                    files::MAX_RECORD_BYTES,
                    budget,
                )?
                .map(|raw| PayloadRepairRecord::decode(raw.bytes()))
                .transpose()
            }
            pub(crate) fn publish_payload_repair(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                selected: &Arc<NativePayloadRepairSelection>,
                record: &PayloadRepairRecord,
                budget: &Deadline,
            ) -> NativeResult<RepairMutationPermit> {
                selected.reverify(self, proof, lock, budget)?;
                record.validate()?;
                if record.selection() != selected.metadata()
                    || record.phase() == PayloadRepairPhase::Retired
                {
                    // Generic publication cannot mint positive copy-retirement evidence.
                    return Err(NativeError::Foreign);
                }
                let context = self.context.clone();
                let lease = lock.0.clone();
                let checked = proof.budget(self, budget)?;
                let document = record.clone();
                let bytes = document.encode()?;
                let output = bytes.clone();
                let owner = self.owner.clone();
                self.owner.run(Dispatch::Mutation, budget, move || {
                    let change = Change::new();
                    let result = change.finish((|| {
                        validate_payload_lock(&context, &lease, &checked)?;
                        let prior = read(
                            &lease.parent,
                            &context,
                            records::RecordName::RepairPayload,
                            &checked,
                        )?;
                        match prior {
                            Some((_, old)) => PayloadRepairRecord::decode(&old)?
                                .publication_successor(&document)?,
                            None if document.phase() == PayloadRepairPhase::Selected => {}
                            None => return Err(NativeError::Foreign),
                        }
                        if let Some(slot) = document.slot() {
                            catalog(&lease.parent, &context, &checked)?
                                .get(slot)
                                .ok_or(NativeError::Foreign)?
                                .matches(&document)?;
                        }
                        publish_fixed(
                            &context,
                            &lease,
                            document.operation(),
                            Target::Record,
                            &bytes,
                            &checked,
                            &change,
                        )?;
                        if document.phase() == PayloadRepairPhase::Complete {
                            let mut history = catalog(&lease.parent, &context, &checked)?;
                            history.complete(
                                document.slot().ok_or(NativeError::Foreign)?,
                                &document,
                            )?;
                            publish_fixed(
                                &context,
                                &lease,
                                document.operation(),
                                Target::Catalog,
                                &history.encode()?,
                                &checked,
                                &change,
                            )?;
                        }
                        Ok(())
                    })());
                    if matches!(result, Err(NativeError::OutcomeUnknown)) {
                        owner.retire_mutations();
                    }
                    result
                })?;
                Ok(RepairMutationPermit {
                    io: self.clone(),
                    bytes: output,
                })
            }
        }
    }

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
            super::repair_archive::refuse_active_repair(context, lease, budget)?;
            super::payload_repair_io::refuse_active(context, lease, budget)?;
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
                self.reject_active_first_install(proof, deadline)?;
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
                self.refuse_unsettled_repair(proof, lock, deadline)?;
                self.refuse_unsettled_payload_repair(proof, lock, deadline)?;
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

    // A8's original-context first-install siblings. None can mint an old-tree completion.
    #[cfg(not(test))]
    mod first_install_io {
        use super::super::super::{
            first_install::{
                FirstInstallDisposition as Disposition,
                record::{FirstInstallRecord, Phase as FirstPhase},
            },
            payload::recovery::{FileStamp, ImageObservation},
        };
        use super::*;
        pub(crate) struct ColdNamespaceAdmission {
            io: Arc<WindowsNativeIo>,
            parent: Arc<Anchor>,
            endpoint: String,
        }
        impl ColdNamespaceAdmission {
            pub(crate) fn token(&self) -> &TokenFacts {
                &self.io.context.target.identity
            }
            pub(crate) fn endpoint(&self) -> &str {
                &self.endpoint
            }
            pub(crate) fn reverify(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                proof.check(&self.io, deadline)?;
                self.parent
                    .revalidate(&self.io.context.security, true, deadline)?;
                self.io.admit_support(deadline)?;
                deadline.check()
            }
            pub(crate) fn reverify_native(&self, deadline: &Deadline) -> NativeResult<()> {
                self.io.context.validate(deadline)?;
                self.parent
                    .revalidate(&self.io.context.security, true, deadline)
            }
        }
        struct ReservationData {
            io: Arc<WindowsNativeIo>,
            parent: Arc<Anchor>,
            agent_identity: FileIdentity,
            agent: Mutex<Option<Arc<File>>>,
            namespace: Mutex<Option<super::super::supervisor_owner::FirstInstallNamespace>>,
            removed_namespace: Mutex<Option<super::first_history_io::RemovedNamespace>>,
            active: AtomicBool,
        }
        #[derive(Clone)]
        pub(crate) struct FirstInstallReservation(Arc<ReservationData>);
        impl FirstInstallReservation {
            pub(super) fn renew_native(
                &self,
                context: &Context,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(context, self.0.io.context.as_ref())
                    || context.target.nonce != self.0.io.context.target.nonce
                    || !self.0.active.load(Ordering::Acquire)
                {
                    return Err(NativeError::Foreign);
                }
                context.validate(deadline)?;
                self.0
                    .parent
                    .revalidate(&context.security, true, deadline)?;
                let agent = self
                    .0
                    .agent
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?
                    .as_ref()
                    .cloned()
                    .ok_or(NativeError::Foreign)?;
                let facts = native::observe(&agent, "agent.lock", &context.security)?;
                files::admit_component(&facts, Admission::PrivateFile)?;
                if facts.identity != self.0.agent_identity {
                    return Err(NativeError::Foreign);
                }
                self.0
                    .namespace
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?
                    .as_ref()
                    .ok_or(NativeError::Foreign)?
                    .reverify_native(deadline)?;
                deadline.check()
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.0.io.as_ref()) || !self.0.active.load(Ordering::Acquire) {
                    return Err(NativeError::Foreign);
                }
                proof.check(io, deadline)?;
                self.0
                    .parent
                    .revalidate(&io.context.security, true, deadline)?;
                let agent = self
                    .0
                    .agent
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?
                    .as_ref()
                    .cloned()
                    .ok_or(NativeError::Foreign)?;
                let facts = native::observe(&agent, "agent.lock", &io.context.security)?;
                files::admit_component(&facts, Admission::PrivateFile)?;
                if facts.identity != self.0.agent_identity {
                    return Err(NativeError::Foreign);
                }
                self.0
                    .namespace
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?
                    .as_ref()
                    .ok_or(NativeError::Foreign)?
                    .reverify(proof, deadline)?;
                if let Some(removed) = self
                    .0
                    .removed_namespace
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .as_ref()
                {
                    removed.reverify(io, proof, deadline)?;
                }
                deadline.check()
            }
            pub(super) fn retain_removed_namespace(
                &self,
                namespace: super::first_history_io::RemovedNamespace,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                self.reverify(&self.0.io, proof, deadline)?;
                namespace.reverify(&self.0.io, proof, deadline)?;
                let mut held = self
                    .0
                    .removed_namespace
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                if held.is_some() {
                    return Err(NativeError::Foreign);
                }
                *held = Some(namespace);
                Ok(())
            }
            /// Called only by the prepared task's actual lock-release boundary. An in-flight
            /// original worker keeps the reservation; neither elapsed time nor a record releases it.
            pub(crate) fn release(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                self.reverify(&self.0.io, proof, deadline)?;
                if !self.0.io.native_idle() {
                    return Err(NativeError::OutcomeUnknown);
                }
                if let Some(removed) = self
                    .0
                    .removed_namespace
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .as_ref()
                {
                    removed.reverify(&self.0.io, proof, deadline)?;
                }
                self.0.active.store(false, Ordering::Release);
                self.0
                    .removed_namespace
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .take();
                self.0
                    .namespace
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .take();
                self.0
                    .agent
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .take();
                deadline.check().map_err(|_| NativeError::OutcomeUnknown)
            }
        }
        pub(crate) struct FirstInstallMutationPermit {
            io: Arc<WindowsNativeIo>,
            reservation: FirstInstallReservation,
            bytes: Vec<u8>,
        }
        impl FirstInstallMutationPermit {
            fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<FirstInstallRecord> {
                if !std::ptr::eq(io, self.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                io.verify_stop_lock(proof, lock, deadline)?;
                self.reservation.reverify(io, proof, deadline)?;
                let record = io
                    .read_first_install(proof, deadline)?
                    .ok_or(NativeError::Missing)?;
                if record.encode()? != self.bytes {
                    return Err(NativeError::Foreign);
                }
                record_matches_context(io, &record)?;
                Ok(record)
            }
        }
        fn renew_intent(
            context: &Context,
            lease: &LockState,
            reservation: &FirstInstallReservation,
            bytes: &[u8],
            deadline: &Deadline,
        ) -> NativeResult<FirstInstallRecord> {
            validate_payload_lock(context, lease, deadline)?;
            reservation.renew_native(context, deadline)?;
            let (_, actual) = lease
                .parent
                .read_private(
                    &PrivateName::new("first-install.json")?,
                    &context.security,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .ok_or(NativeError::Missing)?;
            if actual != bytes {
                return Err(NativeError::Foreign);
            }
            FirstInstallRecord::decode(&actual)
        }
        fn context_bytes(io: &WindowsNativeIo) -> NativeResult<Vec<u8>> {
            serde_json::to_vec(
                &super::super::super::payload::recovery::OuterContextCorrelation::new(
                    io.target().identity(),
                )?,
            )
            .map_err(|_| NativeError::Invalid)
        }
        fn record_matches_context(
            io: &WindowsNativeIo,
            record: &FirstInstallRecord,
        ) -> NativeResult<()> {
            if record.context() != context_bytes(io)? {
                return Err(NativeError::Foreign);
            }
            record.validate()
        }
        fn record_matches_user(
            io: &WindowsNativeIo,
            record: &FirstInstallRecord,
        ) -> NativeResult<()> {
            let old: super::super::super::payload::recovery::OuterContextCorrelation =
                serde_json::from_slice(record.context()).map_err(|_| NativeError::Invalid)?;
            old.same_user(io.target().identity())?;
            record.validate()
        }
        impl WindowsNativeIo {
            /// Additive original-lock validation for the first-only service sibling.
            pub(crate) fn verify_first_lock(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                self.verify_stop_lock(proof, lock, deadline)
            }

            pub(crate) fn first_install_context(&self) -> NativeResult<Vec<u8>> {
                context_bytes(self)
            }
            pub(crate) fn read_first_install(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<Option<FirstInstallRecord>> {
                self.read_record(
                    proof,
                    records::RecordName::FirstInstall,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .map(|r| FirstInstallRecord::decode(r.bytes()))
                .transpose()
            }
            /// Fixed private record inventory only. Preview has no lock, namespace or effect.
            pub(crate) fn preview_first_install(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<Disposition> {
                use super::super::super::{
                    first_install::{DecodedHistory, classify_history},
                    service::journal::Journal,
                };
                let history = self.read_first_history_intent(proof, deadline)?;
                if let Some(history) = &history {
                    if !history.complete {
                        return Ok(Disposition::CompletedRemoval);
                    }
                    self.verify_first_history(proof, history, deadline)?;
                }
                let removal = self.read_removal(proof, deadline)?;
                let first = self.read_first_install(proof, deadline)?;
                if history.is_some() && first.is_none() {
                    return Ok(Disposition::CompletedRemoval);
                }
                if let Some(removal) = removal.as_ref() {
                    return Ok(classify_history(DecodedHistory {
                        removal: Some(removal.cursor()),
                        first: first.as_ref().map(|r| r.phase()),
                        journal: None,
                        logon: false,
                        claimed_activation: false,
                        names: &[],
                    }));
                }
                if let Some(first) = &first
                    && (first.phase() == FirstPhase::Complete
                        || self.first_source_settled(proof, first, deadline)?)
                {
                    return Ok(Disposition::Existing);
                }
                if let Some(record) = &first {
                    record_matches_user(self, record)?;
                }
                let journal = Journal::read(self, proof, deadline)?;
                let logon =
                    super::super::activation::SupervisorLogonRecord::read(self, proof, deadline)?;
                let task = self
                    .read_record(
                        proof,
                        records::RecordName::TaskActivation,
                        files::MAX_RECORD_BYTES,
                        deadline,
                    )?
                    .map(|r| super::super::activation::TaskActivationRecord::decode(r.bytes()))
                    .transpose()?;
                // Epoch names are diagnostic lineage only, but malformed epoch bytes still refuse.
                for slot in 0..3 {
                    if let Some(epoch) = self.read_record(
                        proof,
                        records::RecordName::SupervisorEpoch(slot),
                        files::MAX_RECORD_BYTES,
                        deadline,
                    )? {
                        Journal::decode(epoch.bytes())?;
                    }
                }
                let context = self.context.clone();
                let budget = proof.budget(self, deadline)?;
                let names = self.owner.run(Dispatch::Observation, deadline, move || {
                    context.validate(&budget)?;
                    match Anchor::open(
                        context.target.paths.installer(),
                        &context.security,
                        true,
                        &budget,
                    )? {
                        Some(parent) => parent.entry_names(&context.security, &budget),
                        None => Ok(Vec::new()),
                    }
                })?;
                let names = if history.is_some() {
                    names
                        .into_iter()
                        .filter(|name| {
                            !matches!(
                                name.as_str(),
                                "first-install-history"
                                    | "first-install-history-index.json"
                                    | "first-install-history-intent.json"
                                    | "first-install-recovery.json"
                            )
                        })
                        .collect::<Vec<_>>()
                } else {
                    names
                };
                Ok(classify_history(DecodedHistory {
                    removal: removal.as_ref().map(|r| r.cursor()),
                    first: first.as_ref().map(|r| r.phase()),
                    journal: journal.as_ref().map(|j| j.phase),
                    logon: logon.is_some(),
                    claimed_activation: task.as_ref().is_some_and(|t| t.claim().is_some()),
                    names: &names,
                }))
            }
            pub(crate) fn reject_active_first_install(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if let Some(record) = self.read_first_install(proof, deadline)?
                    && record.phase() != FirstPhase::Complete
                    && !self.first_source_settled(proof, &record, deadline)?
                {
                    return Err(NativeError::Busy);
                }
                Ok(())
            }
            /// A fresh operation owns at most one backup generation. Unknown older material
            /// is retained and reported incomplete, never adopted as a prune capability.
            pub(crate) fn first_retention_complete(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                record: &FirstInstallRecord,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                self.verify_stop_lock(proof, lock, deadline)?;
                record_matches_user(self, record)?;
                if record.phase() != FirstPhase::PruneIntent
                    || self.read_first_install(proof, deadline)?.as_ref() != Some(record)
                {
                    return Err(NativeError::Foreign);
                }
                let root = self.payload_root(proof, lock, deadline)?.0;
                let context = self.context.clone();
                let operation = record.operation();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let root = PayloadRoot(root)
                        .check(&context, &budget)?
                        .ok_or(NativeError::Missing)?;
                    let Some(backups) =
                        root.child("first-install-backups", &context.security, &budget)?
                    else {
                        return Ok(true);
                    };
                    let names = backups.entry_names(&context.security, &budget)?;
                    Ok(names.iter().all(|name| name == &records::hex(&operation)))
                })
            }
            pub(crate) fn reserve_first_install(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<FirstInstallReservation> {
                self.verify_stop_lock(proof, lock, deadline)?;
                if !matches!(
                    self.preview_first_install(proof, deadline)?,
                    Disposition::Eligible | Disposition::Resume
                ) {
                    return Err(NativeError::Unsupported);
                }
                self.reserve_first_namespace(proof, lock, deadline)
            }
            // Shared mechanical acquisition only. Callers have distinct policy/proof factories;
            // the existing cold entry still requires its original Eligible/Resume gate above.
            pub(super) fn reserve_first_namespace(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<FirstInstallReservation> {
                self.acquire_first_reservation(proof, lock, true, deadline)
            }
            pub(super) fn acquire_first_reservation(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                require_task_absence: bool,
                deadline: &Deadline,
            ) -> NativeResult<FirstInstallReservation> {
                self.verify_stop_lock(proof, lock, deadline)?;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let budget = proof.budget(self, deadline)?;
                let (parent, agent, agent_identity, endpoint) =
                    self.owner.run(Dispatch::Mutation, deadline, move || {
                        let change = Change::new();
                        change.finish((|| {
                            validate_payload_lock(&context, &lease, &budget)?;
                            let check = || context.validate(&budget);
                            let scheduler = super::super::task::Scheduler::connect_repair(&check)
                                .map_err(|_| NativeError::Unavailable)?;
                            if require_task_absence
                                && scheduler
                                    .repair_snapshot(&check)
                                    .map_err(|_| NativeError::Unavailable)?
                                    .is_some()
                            {
                                return Err(NativeError::Foreign);
                            }
                            let parent = Arc::new(
                                Anchor::open(
                                    &format!("{}\\Crosspane", context.target.paths.local()),
                                    &context.security,
                                    true,
                                    &budget,
                                )?
                                .ok_or(NativeError::Missing)?,
                            );
                            let canonical =
                                parent.canonical_dos_path(&context.security, &budget)?;
                            let runtime = format!(
                                "{}\\runtime",
                                canonical.to_str().ok_or(NativeError::Unsupported)?
                            );
                            let mut key = runtime.as_bytes().to_vec();
                            key.extend_from_slice(context.target.identity.user.bytes());
                            key.extend_from_slice(context.target.identity.logon.bytes());
                            key.extend_from_slice(&context.target.identity.session.to_le_bytes());
                            let endpoint = format!(
                                r"\\.\pipe\Crosspane.Installer.Supervisor.{:016x}",
                                xxhash_rust::xxh3::xxh3_64(&key)
                            );
                            change.reached();
                            let agent =
                                parent.first_install_agent_lock(&context.security, &budget)?;
                            agent.try_lock().map_err(|error| match error {
                                std::fs::TryLockError::WouldBlock => NativeError::Busy,
                                _ => NativeError::Unavailable,
                            })?;
                            let facts = native::observe(&agent, "agent.lock", &context.security)?;
                            files::admit_component(&facts, Admission::PrivateFile)?;
                            budget.check()?;
                            Ok((parent, Arc::new(agent), facts.identity, endpoint))
                        })())
                    })?;
                let admission = Arc::new(ColdNamespaceAdmission {
                    io: self.clone(),
                    parent: parent.clone(),
                    endpoint,
                });
                let namespace = super::super::supervisor_owner::FirstInstallNamespace::reserve(
                    admission, proof, deadline,
                )?;
                let reservation = FirstInstallReservation(Arc::new(ReservationData {
                    io: self.clone(),
                    parent,
                    agent_identity,
                    agent: Mutex::new(Some(agent)),
                    namespace: Mutex::new(Some(namespace)),
                    removed_namespace: Mutex::new(None),
                    active: AtomicBool::new(true),
                }));
                reservation.reverify(self, proof, deadline)?;
                Ok(reservation)
            }
            pub(crate) fn publish_first_install(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstInstallReservation,
                record: &FirstInstallRecord,
                deadline: &Deadline,
            ) -> NativeResult<FirstInstallMutationPermit> {
                self.verify_stop_lock(proof, lock, deadline)?;
                reservation.reverify(self, proof, deadline)?;
                record_matches_context(self, record)?;
                if let Some(old) = self.read_first_install(proof, deadline)? {
                    if !record.same_selection(&old) {
                        match super::super::super::first_install::reopen(
                            &old,
                            self.target().identity(),
                        )? {
                            super::super::super::first_install::Reopen::Restart(expected)
                                if expected == *record => {}
                            _ => return Err(NativeError::Foreign),
                        }
                    }
                    if record.phase().rank() < old.phase().rank()
                        || old.phase() == FirstPhase::Unknown
                    {
                        return Err(NativeError::Foreign);
                    }
                } else if record.phase() != FirstPhase::Intent {
                    return Err(NativeError::Foreign);
                }
                let bytes = record.encode()?;
                let publication = self.publish_record(
                    proof,
                    lock,
                    records::RecordName::FirstInstall,
                    &bytes,
                    deadline,
                )?;
                if publication.native_failure.is_some()
                    || publication.state != records::PublicationRecovery::NewPublished
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                Ok(FirstInstallMutationPermit {
                    io: self.clone(),
                    reservation: reservation.clone(),
                    bytes,
                })
            }
            #[allow(clippy::too_many_arguments)] // Separate genuine context, lock, intent and pin.
            pub(crate) fn stage_first_install(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &FirstInstallMutationPermit,
                role: PayloadRole,
                input: Box<dyn std::io::Read + Send>,
                expected: &ApprovedPe,
                deadline: &Deadline,
            ) -> NativeResult<StagedPe> {
                let record = permit.reverify(self, proof, lock, deadline)?;
                if record.phase() != FirstPhase::StageIntent(role)
                    || expected.role() != role
                    || expected.facts() != &record.role(role)?.approved
                {
                    return Err(NativeError::Foreign);
                }
                let root = self.payload_root(proof, lock, deadline)?.0;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let reservation = permit.reservation.clone();
                let bytes = permit.bytes.clone();
                let expected = expected.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let change = Change::new();
                    change.finish((|| {
                        let record = renew_intent(&context, &lease, &reservation, &bytes, &budget)?;
                        let root = PayloadRoot(root).ensure(&context, &budget, &change)?;
                        let stages = ensure_payload_child(
                            &root,
                            "first-install-stage",
                            &context,
                            &budget,
                            &change,
                        )?;
                        let parent = Arc::new(ensure_payload_child(
                            &stages,
                            &records::hex(&record.operation()),
                            &context,
                            &budget,
                            &change,
                        )?);
                        change.reached();
                        let image = parent.stage_image(
                            role.leaf(),
                            input,
                            &expected,
                            &context.security,
                            &budget,
                        )?;
                        reservation.renew_native(&context, &budget)?;
                        Ok(StagedPe {
                            target: context.target.nonce,
                            operation: record.operation(),
                            role,
                            parent,
                            image,
                            expected,
                        })
                    })())
                })
            }
            pub(crate) fn observe_first_stage(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                record: &FirstInstallRecord,
                role: PayloadRole,
                expected: &ApprovedPe,
                deadline: &Deadline,
            ) -> NativeResult<Option<ImageObservation>> {
                self.verify_stop_lock(proof, lock, deadline)?;
                record_matches_context(self, record)?;
                let root = self.payload_root(proof, lock, deadline)?.0;
                let context = self.context.clone();
                let operation = record.operation();
                let expected = expected.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let Some(root) = PayloadRoot(root).check(&context, &budget)? else {
                        return Ok(None);
                    };
                    let Some(stages) =
                        root.child("first-install-stage", &context.security, &budget)?
                    else {
                        return Ok(None);
                    };
                    let Some(parent) =
                        stages.child(&records::hex(&operation), &context.security, &budget)?
                    else {
                        return Ok(None);
                    };
                    if parent
                        .opaque(role.leaf(), false, &context.security, &budget)?
                        .is_none()
                    {
                        return Ok(None);
                    }
                    let image = parent.open_image(
                        role.leaf(),
                        true,
                        expected.version(),
                        &context.security,
                        &budget,
                    )?;
                    if image.facts != *expected.facts() {
                        return Err(NativeError::Foreign);
                    }
                    Ok(Some(ImageObservation {
                        identity: image.identity.into(),
                        facts: image.facts,
                    }))
                })
            }
            pub(crate) fn backup_first_install(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &FirstInstallMutationPermit,
                role: PayloadRole,
                deadline: &Deadline,
            ) -> NativeResult<Option<FileStamp>> {
                let record = permit.reverify(self, proof, lock, deadline)?;
                if record.phase() != FirstPhase::BackupIntent(role) {
                    return Err(NativeError::Foreign);
                }
                let root = self.payload_root(proof, lock, deadline)?.0;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let reservation = permit.reservation.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let change = Change::new();
                    change.finish((|| {
                        let record = renew_intent(&context, &lease, &reservation, &bytes, &budget)?;
                        let root = PayloadRoot(root)
                            .check(&context, &budget)?
                            .ok_or(NativeError::Missing)?;
                        let actual = root.opaque(role.leaf(), true, &context.security, &budget)?;
                        match (record.role(role)?.original, actual) {
                            (
                                super::super::super::payload::recovery::OriginalLeaf::Missing,
                                None,
                            ) => Ok(None),
                            (
                                super::super::super::payload::recovery::OriginalLeaf::Present(id),
                                Some(actual),
                            ) if FileStamp::from(actual.identity) == id => {
                                let backups = ensure_payload_child(
                                    &root,
                                    "first-install-backups",
                                    &context,
                                    &budget,
                                    &change,
                                )?;
                                let parent = ensure_payload_child(
                                    &backups,
                                    &records::hex(&record.operation()),
                                    &context,
                                    &budget,
                                    &change,
                                )?;
                                change.reached();
                                let moved = root.move_opaque(
                                    actual,
                                    &parent,
                                    role.leaf(),
                                    &context.security,
                                    &budget,
                                )?;
                                let seen = parent
                                    .opaque(role.leaf(), false, &context.security, &budget)?
                                    .ok_or(NativeError::OutcomeUnknown)?;
                                if seen.identity != moved
                                    || root
                                        .opaque(role.leaf(), false, &context.security, &budget)?
                                        .is_some()
                                {
                                    return Err(NativeError::OutcomeUnknown);
                                }
                                Ok(Some(moved.into()))
                            }
                            _ => Err(NativeError::Foreign),
                        }
                    })())
                })
            }
            pub(crate) fn observe_first_backup(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                record: &FirstInstallRecord,
                role: PayloadRole,
                deadline: &Deadline,
            ) -> NativeResult<Option<FileStamp>> {
                self.verify_stop_lock(proof, lock, deadline)?;
                record_matches_context(self, record)?;
                let root = self.payload_root(proof, lock, deadline)?.0;
                let context = self.context.clone();
                let operation = record.operation();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let Some(root) = PayloadRoot(root).check(&context, &budget)? else {
                        return Ok(None);
                    };
                    let Some(backups) =
                        root.child("first-install-backups", &context.security, &budget)?
                    else {
                        return Ok(None);
                    };
                    let Some(parent) =
                        backups.child(&records::hex(&operation), &context.security, &budget)?
                    else {
                        return Ok(None);
                    };
                    Ok(parent
                        .opaque(role.leaf(), false, &context.security, &budget)?
                        .map(|leaf| leaf.identity.into()))
                })
            }
            pub(crate) fn publish_first_image(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &FirstInstallMutationPermit,
                staged: StagedPe,
                deadline: &Deadline,
            ) -> NativeResult<OpenedPe> {
                let record = permit.reverify(self, proof, lock, deadline)?;
                if record.phase() != FirstPhase::PublishIntent(staged.role)
                    || staged.operation != record.operation()
                    || staged.target != self.context.target.nonce
                    || record.role(staged.role)?.staged.as_ref() != Some(&staged.observation())
                {
                    return Err(NativeError::Foreign);
                }
                let root = self.payload_root(proof, lock, deadline)?.0;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let reservation = permit.reservation.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let change = Change::new();
                    change.finish((|| {
                        renew_intent(&context, &lease, &reservation, &bytes, &budget)?;
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
                        let identity = staged.image.identity;
                        let expected = staged.expected;
                        let role = staged.role;
                        drop(staged.image);
                        let image = root.open_image(
                            role.leaf(),
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
                            leaf: role.leaf().into(),
                            image,
                            expected,
                        })))
                    })())
                })
            }
        }
    }
    #[cfg(not(test))]
    pub(crate) use first_install_io::{
        ColdNamespaceAdmission, FirstInstallMutationPermit, FirstInstallReservation,
    };

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
                // A present cold record is correlation only. The genuine task claim and
                // kernel-exclusive owner above remain the sole fresh launch admission.
                #[cfg(not(test))]
                if let Some(first) = self.read_first_install(proof, deadline)? {
                    use super::super::first_install::record::Phase as FirstPhase;
                    if first.operation() != permit.operation()
                        || !matches!(
                            first.phase(),
                            FirstPhase::RunIntent | FirstPhase::RunObserved
                        )
                    {
                        return Err(NativeError::Foreign);
                    }
                }
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
        /// Repair requires an exact completed predecessor; it cannot take the old first-epoch
        /// or upgrade path. Caller retains the actual native task/namespace before any file effect.
        #[cfg(not(test))]
        pub(crate) fn prepare_repair_epoch(
            self: &Arc<Self>,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &Arc<super::super::service::task::TaskRunPermit>,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<RepairArchiveResult> {
            self.verify_stop_lock(proof, lock, deadline)?;
            if !permit.is_repair() {
                return Err(NativeError::Foreign);
            }
            permit.reverify(self, proof, deadline)?;
            let namespace = owner.exclusive_lease(proof, deadline)?;
            namespace.reverify(self, proof, deadline)?;
            self.check_completed_archive_intent(proof, deadline)?;
            let source = self
                .read_record(
                    proof,
                    records::RecordName::Supervisor,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .ok_or(NativeError::Foreign)?;
            let prior = super::super::service::journal::Journal::decode(source.bytes())?;
            let lineage = super::activation::correlate_repair(
                permit.operation(),
                permit.user(),
                permit.repair_selection()?,
                &prior,
            )?;
            if !lineage.matches_predecessor(&prior) || lineage.operation() != permit.operation() {
                return Err(NativeError::Foreign);
            }
            let claim = EpochClaim::Repair(permit);
            claim.renew_predecessor(self, proof, &prior, deadline)?;
            let archived = self
                .archive_epoch_files(proof, lock, &namespace, claim, &prior, &source, deadline)?;
            publish_epoch_preparing(
                self,
                proof,
                lock,
                claim.epoch(self)?,
                Some(&archived.intent),
                deadline,
            )?;
            let result = RepairArchiveResult {
                io: self.clone(),
                namespace,
                permit: permit.clone(),
                archived,
            };
            result.reverify(self, proof, lock, permit, owner, deadline)?;
            Ok(result)
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
            self.reject_active_first_install(proof, deadline)?;
            self.refuse_unsettled_repair(proof, lock, deadline)?;
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
        // A6 actual read-only namespace hold. Copies must already be positively absent;
        // no process/tree completion, cleanup/delete routine or image/start approval is exposed.
        pub(super) struct RepairNamespaceHold(Arc<ExclusiveKeeperNamespace>);
        pub(super) fn reserve_repair_namespace(
            io: Arc<WindowsNativeIo>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<RepairNamespaceHold> {
            ExclusiveKeeperNamespace::reserve(io, proof, deadline).map(RepairNamespaceHold)
        }
        impl RepairNamespaceHold {
            pub(super) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.0.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                self.0.reverify(proof, deadline)
            }
            pub(super) fn reverify_bound(
                &self,
                context: &Context,
                budget: &Deadline,
            ) -> NativeResult<()> {
                if self.0.io.context.target.nonce != context.target.nonce {
                    return Err(NativeError::Foreign);
                }
                context.validate(budget)?;
                self.0.own.reverify(budget)?;
                let mut flags = 0;
                // SAFETY: actual retained FIRST_INSTANCE pipe object, bounded read-only query;
                // no client, recorded handle, PID/name reconstruction or kernel mutation.
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
            // Transfer only a hold on the already-owned first-instance object. The
            // archive terminal/permit renews the exact metadata after selection.
            pub(super) fn archive_namespace(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<Arc<RepairNamespaceHold>> {
                self.reverify(io, proof, lock, deadline)?;
                Ok(Arc::new(RepairNamespaceHold(self.namespace.clone())))
            }
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
            io.refuse_unsettled_repair(proof, lock, deadline)?;
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
            // Set only by the existing authenticated Commit exchange, never by admit/records.
            committed_here: bool,
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
                    committed_here: true,
                })
            }
        }
        impl KeeperContinuation {
            pub(crate) fn integration_handoff(
                &self,
                deadline: &Deadline,
            ) -> NativeResult<super::super::super::service::IntegrationHandoff> {
                use super::super::super::integration::domains::CommitObservation;
                Ok(super::super::super::service::integration_observe_handoff(
                    self.committed_here,
                    deadline,
                    |slice| {
                        self.observe(slice).map(|observed| match observed.stage {
                            KeeperStage::Committed => CommitObservation::Committed,
                            KeeperStage::Complete => CommitObservation::Complete,
                            KeeperStage::Retained => CommitObservation::Retained,
                            KeeperStage::Ready | KeeperStage::Preparing => CommitObservation::Ready,
                            _ => CommitObservation::Refused,
                        })
                    },
                ))
            }
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
                    committed_here: false,
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

    #[cfg(not(test))]
    mod first_history_io {
        use super::super::super::first_install::record::Phase as FirstPhase;
        use super::super::super::{
            first_install::{
                driver::{self, FirstHistoryPort, HistoryObservation},
                record::*,
            },
            payload::{helper::removal::RemovalKeeperLease, recovery::FileStamp},
            removal::{RemovalCursor, RemovalRecord},
            service::{
                journal::{Journal, Phase as JournalPhase},
                task::TaskRunPermit,
            },
        };
        use super::super::{
            activation::{Phase as TaskPhase, SupervisorLogonRecord, TaskActivationRecord},
            jobs::SupervisorOwner,
            supervisor_owner::ExclusiveSupervisorLease,
        };
        use super::*;

        pub(super) struct RemovedNamespace {
            // Drop the server and lease before its runtime. No request or worker is started here.
            server: tokio::net::windows::named_pipe::NamedPipeServer,
            lease: Arc<RemovalKeeperLease>,
            runtime: tokio::runtime::Runtime,
            operation: [u8; 16],
        }
        impl RemovedNamespace {
            pub(super) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                let _ = (&self.server, &self.runtime);
                self.lease.reverify(io, proof, self.operation, deadline)
            }
        }
        pub(crate) struct CompletedRemovalHistory {
            io: Arc<WindowsNativeIo>,
            reservation: FirstInstallReservation,
            leaves: Vec<(HistoryLeaf, Vec<u8>)>,
            operation: [u8; 16],
            context: Vec<u8>,
            resume: Option<FirstHistoryIntent>,
        }
        pub(crate) struct ArchivedFirstHistory {
            io: Arc<WindowsNativeIo>,
            reservation: FirstInstallReservation,
            intent: FirstHistoryIntent,
        }
        impl ArchivedFirstHistory {
            pub(crate) fn reservation(&self) -> &FirstInstallReservation {
                &self.reservation
            }
            // reason: a reinstall runs under next_operation, not the removed operation.
            #[allow(clippy::misnamed_getters)]
            pub(crate) fn operation(&self) -> [u8; 16] {
                self.intent.selected.next_operation
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
                self.reservation.reverify(io, proof, deadline)?;
                io.verify_first_history(proof, &self.intent, deadline)
            }
        }
        pub(crate) struct FirstInstallArchiveResult {
            io: Arc<WindowsNativeIo>,
            namespace: Arc<ExclusiveSupervisorLease>,
            permit: Arc<TaskRunPermit>,
            intent: FirstHistoryIntent,
        }
        impl FirstInstallArchiveResult {
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &Arc<TaskRunPermit>,
                owner: &Arc<SupervisorOwner>,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref())
                    || !Arc::ptr_eq(permit, &self.permit)
                    || !Arc::ptr_eq(&self.namespace, &owner.exclusive_lease(proof, deadline)?)
                    || permit.operation() != self.intent.selected.next_operation
                {
                    return Err(NativeError::Foreign);
                }
                io.verify_stop_lock(proof, lock, deadline)?;
                self.namespace.reverify(io, proof, deadline)?;
                permit.reverify(io, proof, deadline)?;
                io.verify_first_history(proof, &self.intent, deadline)?;
                preparing_matches(io, proof, &EpochClaim::Task(permit).epoch(io)?, deadline)?;
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
                Ok(())
            }
            pub(crate) fn publish_bound(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &Arc<TaskRunPermit>,
                owner: &Arc<SupervisorOwner>,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref())
                    || !Arc::ptr_eq(permit, &self.permit)
                    || !Arc::ptr_eq(&self.namespace, &owner.exclusive_lease(proof, deadline)?)
                {
                    return Err(NativeError::Foreign);
                }
                io.verify_first_history(proof, &self.intent, deadline)?;
                publish_epoch_bound(
                    io,
                    proof,
                    lock,
                    &self.namespace,
                    EpochClaim::Task(permit),
                    owner,
                    deadline,
                )
            }
        }
        fn validate_slot_document(
            slot: &FirstHistorySlot,
            leaves: &[(HistoryLeaf, Vec<u8>)],
            user: &TokenFacts,
        ) -> NativeResult<()> {
            match slot.source {
                FirstHistorySource::Removal => {
                    let (r, _) = decode_removal_history(leaves, user)?;
                    if r.operation() != slot.operation {
                        return Err(NativeError::Foreign);
                    }
                }
                FirstHistorySource::Partial | FirstHistorySource::Stale => {
                    let source = leaves
                        .iter()
                        .find(|(l, _)| l.leaf == "first-install.json")
                        .ok_or(NativeError::Foreign)?;
                    let first = FirstInstallRecord::decode(&source.1)?;
                    let old: super::super::super::payload::recovery::OuterContextCorrelation =
                        serde_json::from_slice(first.context())
                            .map_err(|_| NativeError::Invalid)?;
                    old.same_user(user)?;
                    if first.operation() != slot.operation
                        || leaves.iter().any(|(l, _)| {
                            !matches!(
                                l.leaf.as_str(),
                                "first-install.json" | "task-activation.json"
                            )
                        })
                    {
                        return Err(NativeError::Foreign);
                    }
                    if let Some((_, bytes)) = leaves
                        .iter()
                        .find(|(l, _)| l.leaf == "task-activation.json")
                    {
                        let activation = TaskActivationRecord::decode(bytes)?;
                        if slot.source != FirstHistorySource::Partial
                            || activation.operation() != first.operation()
                            || activation.claim().is_some()
                        {
                            return Err(NativeError::Foreign);
                        }
                    }
                }
            }
            Ok(())
        }
        fn core_names() -> Vec<records::RecordName> {
            use records::RecordName as N;
            vec![
                N::Supervisor,
                N::SupervisorLogon,
                N::TaskActivation,
                N::SupervisorEpoch(0),
                N::SupervisorEpoch(1),
                N::SupervisorEpoch(2),
                N::SupervisorArchiveIntent,
                N::Removal,
                N::FirstInstall,
                N::FirstInstallRecovery,
            ]
        }
        fn decode_removal_history(
            leaves: &[(HistoryLeaf, Vec<u8>)],
            user: &TokenFacts,
        ) -> NativeResult<(RemovalRecord, super::super::activation::EpochProvenance)> {
            let bytes = |name: &str| -> NativeResult<&[u8]> {
                leaves
                    .iter()
                    .find(|(leaf, _)| leaf.leaf == name)
                    .map(|(_, bytes)| bytes.as_slice())
                    .ok_or(NativeError::Foreign)
            };
            let removal = RemovalRecord::decode(bytes("removal.json")?)?;
            removal.context().same_user(user)?;
            if !matches!(
                removal.cursor(),
                RemovalCursor::Retired
                    | RemovalCursor::Complete {
                        retained_copy: None
                    }
            ) {
                return Err(NativeError::OutcomeUnknown);
            }
            let journal = Journal::decode(bytes("supervisor.json")?)?;
            let provenance = SupervisorLogonRecord::decode(bytes("supervisor-logon.json")?)?;
            let epoch = provenance.matches_current(&journal)?.clone();
            if journal.phase != JournalPhase::Finished
                || journal.user != user.user.sddl()
                || removal
                    .stopped()
                    .is_none_or(|stopped| journal.current != Some(stopped.generation))
            {
                return Err(NativeError::Foreign);
            }
            let task = TaskActivationRecord::decode(bytes("task-activation.json")?)?;
            if task.phase() != TaskPhase::RunObserved || task.claim().is_none() {
                return Err(NativeError::Foreign);
            }
            // Correlation only: compare the exact decoded historical claim to matched provenance.
            // No saved PID is opened, and no image is approved here.
            let claim = task.claim().ok_or(NativeError::Foreign)?;
            if task.user() != epoch.user() {
                return Err(NativeError::Foreign);
            }
            let mut history = Vec::new();
            for slot in 0..3 {
                if let Some((leaf, data)) = leaves
                    .iter()
                    .find(|(leaf, _)| leaf.leaf == format!("supervisor-epoch-{slot}.json"))
                {
                    let prior = Journal::decode(data)?;
                    history.push(
                        provenance
                            .matches_history(slot, &prior, leaf.identity, leaf.digest)?
                            .clone(),
                    );
                }
            }
            // The archive keeps three slots and, once full, evicts the lowest clock epoch. That
            // clock epoch equals owner_creation (EpochProvenance::new and validate). A claim must
            // match a retained epoch unless three entries are retained and the claim is older than
            // all of them; then it may have been evicted legitimately. Only the epoch match is
            // waived in that case. Each archived leaf above was still verified byte-exact.
            let may_be_retained = history.len() < 3
                || history
                    .iter()
                    .map(|entry| entry.owner_creation())
                    .min()
                    .is_none_or(|oldest| claim.creation >= oldest);
            let mut epochs = vec![epoch.clone()];
            epochs.extend(history);
            if may_be_retained
                && !epochs.iter().any(|epoch| {
                    serde_json::to_value(epoch).ok().is_some_and(|facts| {
                        epoch.operation() == task.operation()
                            && facts.get("owner_pid").and_then(serde_json::Value::as_u64)
                                == Some(u64::from(claim.pid))
                            && epoch.owner_creation() == claim.creation
                    })
                })
            {
                return Err(NativeError::Foreign);
            }
            for (leaf, data) in leaves {
                match leaf.leaf.as_str() {
                    "first-install.json" => {
                        let first = FirstInstallRecord::decode(data)?;
                        if first.phase() != FirstPhase::Complete {
                            let (source, recovery) = leaves
                                .iter()
                                .find(|(l, _)| l.leaf == "first-install-recovery.json")
                                .ok_or(NativeError::OutcomeUnknown)?;
                            source.validate()?;
                            let recovery = FirstRecoveryRecord::decode(recovery)?;
                            if recovery.mode != FirstRecoveryMode::Supersede
                                || recovery.cursor != FirstRecoveryCursor::Superseded
                                || recovery.document != first
                                || !recovery.source.matches(leaf.identity, data)
                            {
                                return Err(NativeError::OutcomeUnknown);
                            }
                        }
                    }
                    "first-install-recovery.json" => {
                        let recovery = FirstRecoveryRecord::decode(data)?;
                        if !matches!(
                            recovery.cursor,
                            FirstRecoveryCursor::Superseded | FirstRecoveryCursor::Retired
                        ) {
                            return Err(NativeError::OutcomeUnknown);
                        }
                    }
                    "supervisor-archive-intent.json" => {
                        let value: super::super::epoch_archive::ArchiveIntent =
                            records::record_data(
                                &records::RecordName::SupervisorArchiveIntent,
                                data,
                            )?;
                        value.require_complete()?;
                    }
                    _ => {}
                }
            }
            validate_history_dependencies(leaves, user)?;
            Ok((removal, epoch))
        }
        fn validate_history_dependencies(
            leaves: &[(HistoryLeaf, Vec<u8>)],
            user: &TokenFacts,
        ) -> NativeResult<()> {
            use super::super::super::{
                payload::recovery as R,
                repair::{payload_record as P, record as M},
            };
            let find = |name: &str| leaves.iter().find(|(leaf, _)| leaf.leaf == name);
            for (leaf, bytes) in leaves {
                match leaf.leaf.as_str() {
                    "stage-catalog.json" => {
                        let catalog: R::StageCatalog =
                            records::record_data(&records::RecordName::StageCatalog, bytes)?;
                        catalog.validate()?;
                        for id in catalog
                            .generations
                            .iter()
                            .map(|g| g.operation)
                            .chain(catalog.active)
                        {
                            let name = records::RecordName::Operation(id);
                            let (_, observed) = find(name.file_name()?.as_str())
                                .ok_or(NativeError::OutcomeUnknown)?;
                            let operation: R::OperationRecord =
                                records::record_data(&name, observed)?;
                            operation.validate()?;
                            if operation.operation() != id
                                || !matches!(
                                    operation.phase(),
                                    R::Phase::Complete | R::Phase::RolledBack
                                )
                            {
                                return Err(NativeError::OutcomeUnknown);
                            }
                        }
                    }
                    "outer-upgrade.json" => {
                        let outer = R::OuterUpgradeRecord::decode(bytes)?;
                        outer.context().same_user(user)?;
                        if !matches!(
                            outer.phase(),
                            R::OuterPhase::Complete | R::OuterPhase::Cancelled
                        ) || outer.copy_cleanup() != R::OuterCopyCleanup::Absent
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                    }
                    "file-recovery.json" => {
                        if R::FileRecoveryJournal::decode(bytes)?.cursor()
                            != R::FileRecoveryCursor::Retired
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                    }
                    "repair.json" => {
                        let record = M::RepairRecord::decode(bytes)?;
                        record.context().same_user(user)?;
                        if record.cursor() != M::RepairCursor::Complete {
                            return Err(NativeError::OutcomeUnknown);
                        }
                    }
                    "repair-evidence-index.json" => {
                        let index = M::EvidenceIndex::decode(bytes)?;
                        if (0..3).any(|slot| {
                            index
                                .get(slot)
                                .is_some_and(|s| s.phase() != M::EvidencePhase::Complete)
                        }) {
                            return Err(NativeError::OutcomeUnknown);
                        }
                    }
                    "repair-publication-intent.json" => {
                        let intent: records::RepairPublicationIntent = records::record_data(
                            &records::RecordName::RepairPublicationIntent,
                            bytes,
                        )?;
                        intent.validate()?;
                        let current = find(intent.target().name().file_name()?.as_str())
                            .map(|(leaf, bytes)| {
                                records::RepairPublicationStamp::new(
                                    epoch_identity(leaf.identity),
                                    bytes,
                                )
                            })
                            .transpose()?;
                        if intent.phase() != records::RepairPublicationPhase::Published
                            || records::recover_repair_publication(&intent, current, None)
                                != records::RepairPublicationObservation::Published
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                    }
                    "repair-payload.json" => {
                        let record = P::PayloadRepairRecord::decode(bytes)?;
                        let (_, bytes) = find("repair-payload-catalog.json")
                            .ok_or(NativeError::OutcomeUnknown)?;
                        let catalog = P::PayloadRepairCatalog::decode(bytes)?;
                        let intent = find("repair-payload-publication-intent.json")
                            .map(|(_, bytes)| P::PayloadRepairPublicationIntent::decode(bytes))
                            .transpose()?;
                        let state = if let Some(intent) = &intent {
                            let current = find(intent.target().name().file_name()?.as_str())
                                .map(|(leaf, bytes)| {
                                    P::PayloadRepairPublicationStamp::new(leaf.identity, bytes)
                                })
                                .transpose()?;
                            P::recover_payload_repair_publication(intent, current, None)
                        } else {
                            P::PayloadRepairPublicationObservation::Published
                        };
                        if !P::retired_cleanup_settled(&record, &catalog, intent.as_ref(), state)? {
                            return Err(NativeError::OutcomeUnknown);
                        }
                    }
                    "repair-payload-catalog.json" => {
                        P::PayloadRepairCatalog::decode(bytes)?.validate()?;
                    }
                    "repair-payload-publication-intent.json" => {
                        P::PayloadRepairPublicationIntent::decode(bytes)?.validate()?;
                    }
                    "repair-pending.json" | "repair-payload-pending.json" => {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        impl WindowsNativeIo {
            pub(crate) fn read_first_history_intent(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<Option<FirstHistoryIntent>> {
                self.read_record(
                    proof,
                    records::RecordName::FirstInstallHistoryIntent,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .map(|record| FirstHistoryIntent::decode(record.bytes()))
                .transpose()
            }
            pub(super) fn read_first_history_index(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<FirstHistoryIndex> {
                self.read_record(
                    proof,
                    records::RecordName::FirstInstallHistoryIndex,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .map(|record| FirstHistoryIndex::decode(record.bytes()))
                .transpose()
                .map(|r| r.unwrap_or_default())
            }
            fn first_history_parent(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<Arc<Anchor>> {
                self.verify_stop_lock(proof, lock, deadline)?;
                Ok(lock.0.parent.clone())
            }
            pub(super) fn validate_first_history_slots(
                &self,
                proof: &SupportProof,
                index: &FirstHistoryIndex,
                active: Option<&FirstHistoryIntent>,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                index.validate()?;
                let context = self.context.clone();
                let index = index.clone();
                let active = active.cloned();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let Some(parent) = Anchor::open(
                        context.target.paths.installer(),
                        &context.security,
                        true,
                        &budget,
                    )?
                    else {
                        if index.slots.iter().any(Option::is_some) || active.is_some() {
                            return Err(NativeError::Foreign);
                        }
                        return Ok(());
                    };
                    for slot in 0..3 {
                        let archive =
                            parent.first_history_slot(slot, false, &context.security, &budget)?;
                        match (&index.slots[usize::from(slot)], archive) {
                            (Some(manifest), Some(archive)) => {
                                let names = archive.entry_names(&context.security, &budget)?;
                                if names.len() != manifest.leaves.len()
                                    || names.iter().any(|n| {
                                        !manifest.leaves.iter().any(|leaf| &leaf.leaf == n)
                                    })
                                {
                                    return Err(NativeError::Foreign);
                                }
                                let mut leaves = Vec::new();
                                for leaf in &manifest.leaves {
                                    let (id, bytes) = archive
                                        .read_private(
                                            &PrivateName::new(&leaf.leaf)?,
                                            &context.security,
                                            files::MAX_RECORD_BYTES,
                                            &budget,
                                        )?
                                        .ok_or(NativeError::Foreign)?;
                                    if !leaf.matches(id.into(), &bytes) {
                                        return Err(NativeError::Foreign);
                                    }
                                    leaves.push((leaf.clone(), bytes));
                                }
                                validate_slot_document(
                                    manifest,
                                    &leaves,
                                    &context.target.identity,
                                )?;
                            }
                            (None, None) => {}
                            (None, Some(archive))
                                if active.as_ref().is_some_and(|intent| {
                                    intent.slot == slot && !intent.complete
                                }) =>
                            {
                                let intent = active.as_ref().ok_or(NativeError::Foreign)?;
                                let names = archive.entry_names(&context.security, &budget)?;
                                if names.iter().any(|n| {
                                    !intent.selected.leaves.iter().any(|leaf| &leaf.leaf == n)
                                }) {
                                    return Err(NativeError::Foreign);
                                }
                            }
                            _ => return Err(NativeError::OutcomeUnknown),
                        }
                    }
                    budget.check()
                })
            }
            pub(crate) fn first_history_capacity_available(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                if let Some(intent) = self.read_first_history_intent(proof, deadline)?
                    && (!intent.complete || self.read_removal(proof, deadline)?.is_none())
                {
                    return Ok(true);
                }
                let index = self.read_first_history_index(proof, deadline)?;
                let active = self.read_first_history_intent(proof, deadline)?;
                self.validate_first_history_slots(proof, &index, active.as_ref(), deadline)?;
                Ok(index.vacant().is_ok())
            }
            pub(crate) fn admit_completed_removal_history(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<CompletedRemovalHistory> {
                self.verify_stop_lock(proof, lock, deadline)?;
                let index = self.read_first_history_index(proof, deadline)?;
                let old = self.read_first_history_intent(proof, deadline)?;
                self.validate_first_history_slots(proof, &index, old.as_ref(), deadline)?;
                let resume = old.filter(|intent| {
                    !intent.complete
                        || self
                            .read_removal(proof, deadline)
                            .is_ok_and(|r| r.is_none())
                });
                let mut leaves = Vec::new();
                if let Some(intent) = &resume {
                    intent.validate()?;
                    // Resuming in a later logon compares only the same user. A different logon
                    // needs the same ended-logon disposition as first-install recovery.
                    let stored: super::super::super::payload::recovery::OuterContextCorrelation =
                        serde_json::from_slice(&intent.selected.context)
                            .map_err(|_| NativeError::Invalid)?;
                    stored.same_user(&self.context.target.identity)?;
                    super::first_recovery_io::renew_prior_correlation(
                        self, proof, &stored, deadline,
                    )?;
                    if index.slots[usize::from(intent.slot)]
                        .as_ref()
                        .is_some_and(|slot| slot != &intent.selected)
                    {
                        return Err(NativeError::Foreign);
                    }
                    let parent = self.first_history_parent(proof, lock, deadline)?;
                    let context = self.context.clone();
                    let intent = intent.clone();
                    let budget = proof.budget(self, deadline)?;
                    leaves = self.owner.run(Dispatch::Observation, deadline, move || {
                        let archive = parent.first_history_slot(
                            intent.slot,
                            false,
                            &context.security,
                            &budget,
                        )?;
                        let mut observed = Vec::new();
                        for leaf in intent.selected.leaves {
                            let name = PrivateName::new(&leaf.leaf)?;
                            let source = parent.read_private(
                                &name,
                                &context.security,
                                files::MAX_RECORD_BYTES,
                                &budget,
                            )?;
                            let destination = archive
                                .as_ref()
                                .map(|a| {
                                    a.read_private(
                                        &name,
                                        &context.security,
                                        files::MAX_RECORD_BYTES,
                                        &budget,
                                    )
                                })
                                .transpose()?
                                .flatten();
                            let (id, bytes) = match (source, destination) {
                                (Some(actual), None) | (None, Some(actual)) => actual,
                                _ => return Err(NativeError::OutcomeUnknown),
                            };
                            if !leaf.matches(id.into(), &bytes) {
                                return Err(NativeError::Foreign);
                            }
                            observed.push((leaf, bytes));
                        }
                        Ok(observed)
                    })?;
                } else {
                    index.vacant()?;
                    for name in core_names() {
                        if let Some(record) = self.read_record(
                            proof,
                            name.clone(),
                            files::MAX_RECORD_BYTES,
                            deadline,
                        )? {
                            leaves.push((
                                HistoryLeaf::observe(
                                    name.file_name()?.as_str().into(),
                                    record.identity.into(),
                                    record.bytes(),
                                )?,
                                record.bytes().to_vec(),
                            ));
                        }
                    }
                }
                let (removal, epoch) =
                    decode_removal_history(&leaves, &self.context.target.identity)?;
                if epoch.authentication_id() != self.context.target.identity.authentication_id {
                    self.query_prior_logon(
                        proof,
                        &MatchedLogonProvenance {
                            io: self.clone(),
                            epoch,
                        },
                        deadline,
                    )?;
                }
                if resume.is_none() {
                    self.collect_terminal_first_dependencies(proof, &mut leaves, deadline)?;
                }
                let reservation = self.reserve_first_namespace(proof, lock, deadline)?;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| NativeError::Unavailable)?;
                let (server, lease) = runtime.block_on(async {
                    RemovalKeeperLease::reserve(self.clone(), proof, removal.operation(), deadline)
                })?;
                reservation.retain_removed_namespace(
                    RemovedNamespace {
                        server,
                        lease,
                        runtime,
                        operation: removal.operation(),
                    },
                    proof,
                    deadline,
                )?;
                let parent = self.first_history_parent(proof, lock, deadline)?;
                let context = self.context.clone();
                let root = self.payload_root(proof, lock, deadline)?.0;
                let operation = removal.operation();
                let repair_operation = leaves
                    .iter()
                    .find(|(leaf, _)| leaf.leaf == "repair-payload.json")
                    .map(|(_, bytes)| {
                        super::super::super::repair::payload_record::PayloadRepairRecord::decode(
                            bytes,
                        )
                        .map(|r| r.operation())
                    })
                    .transpose()?;
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    if PayloadRoot(root).check(&context, &budget)?.is_some() {
                        return Err(NativeError::Foreign);
                    }
                    let state = Anchor::open(
                        &format!("{}\\Crosspane", context.target.paths.local()),
                        &context.security,
                        true,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?;
                    if let Some(runtime) = state.child("runtime", &context.security, &budget)?
                        && let Some(copies) =
                            runtime.child("removal", &context.security, &budget)?
                        && let Some(op) =
                            copies.child(&records::hex(&operation), &context.security, &budget)?
                    {
                        for leaf in ["keeper-copy.exe", "helper-copy.exe"] {
                            if op
                                .opaque(leaf, false, &context.security, &budget)?
                                .is_some()
                            {
                                return Err(NativeError::OutcomeUnknown);
                            }
                        }
                    }
                    if let Some(operation) = repair_operation
                        && let Some(runtime) = state.child("runtime", &context.security, &budget)?
                        && let Some(repairs) =
                            runtime.child("repair", &context.security, &budget)?
                        && let Some(op) =
                            repairs.child(&records::hex(&operation), &context.security, &budget)?
                        && op
                            .opaque("keeper-copy.exe", false, &context.security, &budget)?
                            .is_some()
                    {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    parent.revalidate(&context.security, true, &budget)
                })?;
                Ok(CompletedRemovalHistory {
                    io: self.clone(),
                    reservation,
                    leaves,
                    operation: removal.operation(),
                    context: self.first_install_context()?,
                    resume,
                })
            }

            fn collect_terminal_first_dependencies(
                &self,
                proof: &SupportProof,
                leaves: &mut Vec<(HistoryLeaf, Vec<u8>)>,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                use super::super::super::{
                    payload::recovery as R,
                    repair::{payload_record as P, record as M},
                };
                let mut add = |name: records::RecordName,
                               bytes: &[u8],
                               id: FileIdentity|
                 -> NativeResult<()> {
                    if leaves.len() >= MAX_HISTORY_LEAVES {
                        return Err(NativeError::Oversize);
                    }
                    let leaf =
                        HistoryLeaf::observe(name.file_name()?.as_str().into(), id.into(), bytes)?;
                    if leaves.iter().any(|(old, _)| old.leaf == leaf.leaf) {
                        return Err(NativeError::Foreign);
                    }
                    leaves.push((leaf, bytes.to_vec()));
                    Ok(())
                };
                let read = |name| self.read_record(proof, name, files::MAX_RECORD_BYTES, deadline);
                if let Some(catalog) = read(records::RecordName::StageCatalog)? {
                    let value: R::StageCatalog =
                        records::record_data(&records::RecordName::StageCatalog, catalog.bytes())?;
                    value.validate()?;
                    let mut operations = value
                        .generations
                        .iter()
                        .map(|g| g.operation)
                        .collect::<Vec<_>>();
                    if let Some(active) = value.active
                        && !operations.contains(&active)
                    {
                        operations.push(active);
                    }
                    if operations.len() > MAX_HISTORY_LEAVES - 1 {
                        return Err(NativeError::Oversize);
                    }
                    for id in operations {
                        let name = records::RecordName::Operation(id);
                        let observed = read(name.clone())?.ok_or(NativeError::OutcomeUnknown)?;
                        let operation: R::OperationRecord =
                            records::record_data(&name, observed.bytes())?;
                        operation.validate()?;
                        if operation.operation() != id
                            || !matches!(
                                operation.phase(),
                                R::Phase::Complete | R::Phase::RolledBack
                            )
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        add(name, observed.bytes(), observed.identity)?;
                    }
                    add(
                        records::RecordName::StageCatalog,
                        catalog.bytes(),
                        catalog.identity,
                    )?;
                }
                if let Some(record) = read(records::RecordName::OuterUpgrade)? {
                    let outer = R::OuterUpgradeRecord::decode(record.bytes())?;
                    outer.context().same_user(&self.context.target.identity)?;
                    if !matches!(
                        outer.phase(),
                        R::OuterPhase::Complete | R::OuterPhase::Cancelled
                    ) || outer.copy_cleanup() != R::OuterCopyCleanup::Absent
                    {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    add(
                        records::RecordName::OuterUpgrade,
                        record.bytes(),
                        record.identity,
                    )?;
                }
                if let Some(record) = read(records::RecordName::FileRecovery)? {
                    let recovery = R::FileRecoveryJournal::decode(record.bytes())?;
                    if recovery.cursor() != R::FileRecoveryCursor::Retired {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    add(
                        records::RecordName::FileRecovery,
                        record.bytes(),
                        record.identity,
                    )?;
                }
                if read(records::RecordName::RepairPending)?.is_some()
                    || read(records::RecordName::RepairPayloadPending)?.is_some()
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                if let Some(record) = read(records::RecordName::Repair)? {
                    let repair = M::RepairRecord::decode(record.bytes())?;
                    repair.context().same_user(&self.context.target.identity)?;
                    if repair.cursor() != M::RepairCursor::Complete {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    add(records::RecordName::Repair, record.bytes(), record.identity)?;
                }
                if let Some(record) = read(records::RecordName::RepairEvidence)? {
                    let index = M::EvidenceIndex::decode(record.bytes())?;
                    for slot in 0..3 {
                        if index
                            .get(slot)
                            .is_some_and(|s| s.phase() != M::EvidencePhase::Complete)
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                    }
                    add(
                        records::RecordName::RepairEvidence,
                        record.bytes(),
                        record.identity,
                    )?;
                }
                if let Some(record) = read(records::RecordName::RepairPublicationIntent)? {
                    let intent: records::RepairPublicationIntent = records::record_data(
                        &records::RecordName::RepairPublicationIntent,
                        record.bytes(),
                    )?;
                    intent.validate()?;
                    let current = read(intent.target().name())?
                        .map(|r| records::RepairPublicationStamp::new(r.identity, r.bytes()))
                        .transpose()?;
                    if intent.phase() != records::RepairPublicationPhase::Published
                        || records::recover_repair_publication(&intent, current, None)
                            != records::RepairPublicationObservation::Published
                    {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    add(
                        records::RecordName::RepairPublicationIntent,
                        record.bytes(),
                        record.identity,
                    )?;
                }
                if let Some(record) = read(records::RecordName::RepairPayload)? {
                    let repair = P::PayloadRepairRecord::decode(record.bytes())?;
                    if repair.phase() != P::PayloadRepairPhase::Retired {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    let catalog = read(records::RecordName::RepairPayloadCatalog)?
                        .ok_or(NativeError::OutcomeUnknown)?;
                    let history = P::PayloadRepairCatalog::decode(catalog.bytes())?;
                    let intent = read(records::RecordName::RepairPayloadPublicationIntent)?
                        .map(|r| P::PayloadRepairPublicationIntent::decode(r.bytes()))
                        .transpose()?;
                    let state = if let Some(intent) = &intent {
                        let current = read(intent.target().name())?
                            .map(|r| {
                                P::PayloadRepairPublicationStamp::new(r.identity.into(), r.bytes())
                            })
                            .transpose()?;
                        P::recover_payload_repair_publication(intent, current, None)
                    } else {
                        P::PayloadRepairPublicationObservation::Published
                    };
                    if !P::retired_cleanup_settled(&repair, &history, intent.as_ref(), state)? {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    add(
                        records::RecordName::RepairPayload,
                        record.bytes(),
                        record.identity,
                    )?;
                    add(
                        records::RecordName::RepairPayloadCatalog,
                        catalog.bytes(),
                        catalog.identity,
                    )?;
                } else if read(records::RecordName::RepairPayloadCatalog)?.is_some()
                    || read(records::RecordName::RepairPayloadPublicationIntent)?.is_some()
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                if let Some(record) = read(records::RecordName::RepairPayloadPublicationIntent)? {
                    let intent = P::PayloadRepairPublicationIntent::decode(record.bytes())?;
                    if intent.phase() != P::PayloadRepairPublicationPhase::Published {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    add(
                        records::RecordName::RepairPayloadPublicationIntent,
                        record.bytes(),
                        record.identity,
                    )?;
                }
                Ok(())
            }
            pub(crate) fn archive_completed_removal(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                history: CompletedRemovalHistory,
                next_operation: [u8; 16],
                deadline: &Deadline,
            ) -> NativeResult<ArchivedFirstHistory> {
                if !Arc::ptr_eq(self, &history.io) {
                    return Err(NativeError::Foreign);
                }
                history.reservation.reverify(self, proof, deadline)?;
                let mut index = self.read_first_history_index(proof, deadline)?;
                let mut intent = match history.resume {
                    Some(intent) => intent,
                    None => index.select(FirstHistorySlot {
                        source: FirstHistorySource::Removal,
                        operation: history.operation,
                        next_operation,
                        context: history.context,
                        leaves: history
                            .leaves
                            .iter()
                            .map(|(leaf, _)| leaf.clone())
                            .collect(),
                    })?,
                };
                let mut port = NativeHistoryPort {
                    io: self,
                    lock,
                    reservation: &history.reservation,
                    deadline,
                };
                driver::archive_history(&mut port, &mut intent, &mut index)?;
                Ok(ArchivedFirstHistory {
                    io: self.clone(),
                    reservation: history.reservation,
                    intent,
                })
            }
            pub(crate) fn verify_first_history(
                &self,
                proof: &SupportProof,
                intent: &FirstHistoryIntent,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                intent.validate()?;
                if !intent.complete
                    || self.read_first_history_intent(proof, deadline)?.as_ref() != Some(intent)
                    || self.read_first_history_index(proof, deadline)?.slots
                        [usize::from(intent.slot)]
                    .as_ref()
                        != Some(&intent.selected)
                {
                    return Err(NativeError::Foreign);
                }
                let context = self.context.clone();
                let selected = intent.clone();
                let budget = proof.budget(self, deadline)?;
                let leaves = self.owner.run(Dispatch::Observation, deadline, move || {
                    let parent = Anchor::open(
                        context.target.paths.installer(),
                        &context.security,
                        true,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?;
                    let archive = parent
                        .first_history_slot(selected.slot, false, &context.security, &budget)?
                        .ok_or(NativeError::Foreign)?;
                    let mut leaves = Vec::new();
                    for leaf in selected.selected.leaves {
                        let (id, bytes) = archive
                            .read_private(
                                &PrivateName::new(&leaf.leaf)?,
                                &context.security,
                                files::MAX_RECORD_BYTES,
                                &budget,
                            )?
                            .ok_or(NativeError::Foreign)?;
                        if !leaf.matches(id.into(), &bytes) {
                            return Err(NativeError::Foreign);
                        }
                        leaves.push((leaf, bytes));
                    }
                    Ok(leaves)
                })?;
                validate_slot_document(&intent.selected, &leaves, &self.context.target.identity)?;
                Ok(())
            }
            pub(crate) fn is_first_reinstall_epoch(
                &self,
                proof: &SupportProof,
                operation: [u8; 16],
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                let Some(intent) = self.read_first_history_intent(proof, deadline)? else {
                    return Ok(false);
                };
                if !intent.complete {
                    return Err(NativeError::OutcomeUnknown);
                }
                Ok(intent.selected.next_operation == operation)
            }
            pub(crate) fn prepare_first_reinstall_epoch(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &Arc<TaskRunPermit>,
                owner: &Arc<SupervisorOwner>,
                deadline: &Deadline,
            ) -> NativeResult<FirstInstallArchiveResult> {
                self.verify_stop_lock(proof, lock, deadline)?;
                permit.reverify(self, proof, deadline)?;
                let namespace = owner.exclusive_lease(proof, deadline)?;
                namespace.reverify(self, proof, deadline)?;
                let intent = self
                    .read_first_history_intent(proof, deadline)?
                    .ok_or(NativeError::Foreign)?;
                self.verify_first_history(proof, &intent, deadline)?;
                let first = self
                    .read_first_install(proof, deadline)?
                    .ok_or(NativeError::Foreign)?;
                if first.operation() != permit.operation()
                    || intent.selected.next_operation != permit.operation()
                    || !matches!(
                        first.phase(),
                        FirstPhase::RunIntent | FirstPhase::RunObserved
                    )
                    || self
                        .read_record(
                            proof,
                            records::RecordName::Supervisor,
                            files::MAX_RECORD_BYTES,
                            deadline,
                        )?
                        .is_some()
                    || SupervisorLogonRecord::read(self, proof, deadline)?.is_some()
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
                let value = FirstInstallArchiveResult {
                    io: self.clone(),
                    namespace,
                    permit: permit.clone(),
                    intent,
                };
                value.reverify(self, proof, lock, permit, owner, deadline)?;
                Ok(value)
            }
            pub(super) fn read_archived_first_source(
                &self,
                proof: &SupportProof,
                expected: &HistoryLeaf,
                operation: [u8; 16],
                deadline: &Deadline,
            ) -> NativeResult<(FileStamp, Vec<u8>)> {
                let index = self.read_first_history_index(proof, deadline)?;
                let active = self.read_first_history_intent(proof, deadline)?;
                let slot = index
                    .slots
                    .iter()
                    .enumerate()
                    .find_map(|(i, slot)| {
                        slot.as_ref()
                            .filter(|s| {
                                s.operation == operation
                                    && s.source != FirstHistorySource::Removal
                                    && s.leaves.contains(expected)
                            })
                            .map(|_| i as u8)
                    })
                    .or_else(|| {
                        active
                            .as_ref()
                            .filter(|i| {
                                i.selected.operation == operation
                                    && i.selected.source != FirstHistorySource::Removal
                                    && i.selected.leaves.contains(expected)
                            })
                            .map(|i| i.slot)
                    })
                    .ok_or(NativeError::Foreign)?;
                let context = self.context.clone();
                let budget = proof.budget(self, deadline)?;
                let expected = expected.clone();
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let parent = Anchor::open(
                        context.target.paths.installer(),
                        &context.security,
                        true,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?;
                    let archive = parent
                        .first_history_slot(slot, false, &context.security, &budget)?
                        .ok_or(NativeError::Foreign)?;
                    let (id, bytes) = archive
                        .read_private(
                            &PrivateName::new(&expected.leaf)?,
                            &context.security,
                            files::MAX_RECORD_BYTES,
                            &budget,
                        )?
                        .ok_or(NativeError::Foreign)?;
                    if !expected.matches(id.into(), &bytes) {
                        return Err(NativeError::Foreign);
                    }
                    Ok((id.into(), bytes))
                })
            }
            pub(super) fn archive_first_recovery_history(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstInstallReservation,
                recovery: &FirstRecoveryRecord,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                use super::super::super::first_install::record::{
                    FirstRecoveryCursor as C, FirstRecoveryMode as M,
                };
                self.verify_stop_lock(proof, lock, deadline)?;
                reservation.reverify(self, proof, deadline)?;
                if recovery.cursor != C::RetireIntent
                    || recovery.pending_scaffold
                    || recovery.pending.iter().any(|p| *p)
                {
                    return Err(NativeError::Foreign);
                }
                let mut index = self.read_first_history_index(proof, deadline)?;
                let old = self.read_first_history_intent(proof, deadline)?;
                self.validate_first_history_slots(proof, &index, old.as_ref(), deadline)?;
                let mut intent = if let Some(old) = old.filter(|i| {
                    i.selected.operation == recovery.document.operation()
                        && i.selected.source != FirstHistorySource::Removal
                }) {
                    if !old.selected.leaves.contains(&recovery.source) {
                        return Err(NativeError::Foreign);
                    }
                    old
                } else {
                    let selected = self
                        .read_record(
                            proof,
                            records::RecordName::FirstInstall,
                            files::MAX_RECORD_BYTES,
                            deadline,
                        )?
                        .ok_or(NativeError::Foreign)?;
                    if !recovery
                        .source
                        .matches(selected.identity.into(), selected.bytes())
                    {
                        return Err(NativeError::Foreign);
                    }
                    let mut leaves = vec![recovery.source.clone()];
                    let source = if matches!(recovery.mode, M::Rollback | M::Remove) {
                        FirstHistorySource::Partial
                    } else {
                        FirstHistorySource::Stale
                    };
                    if source == FirstHistorySource::Partial {
                        if let Some(record) = self.read_record(
                            proof,
                            records::RecordName::TaskActivation,
                            files::MAX_RECORD_BYTES,
                            deadline,
                        )? {
                            let task = TaskActivationRecord::decode(record.bytes())?;
                            if task.operation() != recovery.document.operation()
                                || task.claim().is_some()
                            {
                                return Err(NativeError::Foreign);
                            }
                            leaves.push(HistoryLeaf::observe(
                                "task-activation.json".into(),
                                record.identity.into(),
                                record.bytes(),
                            )?);
                        }
                        if self
                            .read_record(
                                proof,
                                records::RecordName::Supervisor,
                                files::MAX_RECORD_BYTES,
                                deadline,
                            )?
                            .is_some()
                            || SupervisorLogonRecord::read(self, proof, deadline)?.is_some()
                        {
                            return Err(NativeError::Foreign);
                        }
                    }
                    let mut next_operation = [0; 16];
                    aws_lc_rs::rand::fill(&mut next_operation)
                        .map_err(|_| NativeError::Unavailable)?;
                    index.select(FirstHistorySlot {
                        source,
                        operation: recovery.document.operation(),
                        next_operation,
                        context: self.first_install_context()?,
                        leaves,
                    })?
                };
                let mut port = NativeHistoryPort {
                    io: self,
                    lock,
                    reservation,
                    deadline,
                };
                driver::archive_history(&mut port, &mut intent, &mut index)?;
                self.verify_first_history(proof, &intent, deadline)
            }
            pub(crate) fn admit_recovered_first_history(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<ArchivedFirstHistory> {
                use super::super::super::first_install::record::{
                    FirstRecoveryCursor as C, FirstRecoveryMode as M,
                };
                self.verify_stop_lock(proof, lock, deadline)?;
                // A stale recovery record must never hijack a reinstall: recovered history is
                // admitted only when no removal or first-install record is live.
                if self.read_removal(proof, deadline)?.is_some()
                    || self.read_first_install(proof, deadline)?.is_some()
                {
                    return Err(NativeError::Foreign);
                }
                let recovery = self
                    .read_first_recovery(proof, deadline)?
                    .ok_or(NativeError::Foreign)?;
                if recovery.cursor != C::Retired
                    || !matches!(recovery.mode, M::Rollback | M::Remove)
                {
                    return Err(NativeError::Foreign);
                }
                let intent = self
                    .read_first_history_intent(proof, deadline)?
                    .ok_or(NativeError::Foreign)?;
                self.verify_first_history(proof, &intent, deadline)?;
                if intent.selected.source != FirstHistorySource::Partial
                    || !intent.selected.leaves.contains(&recovery.source)
                {
                    return Err(NativeError::Foreign);
                }
                let reservation = self.acquire_first_reservation(proof, lock, true, deadline)?;
                // Archive metadata cannot prove file completion: independently renew each actual triad.
                self.verify_recovered_first_files(proof, lock, &reservation, &recovery, deadline)?;
                Ok(ArchivedFirstHistory {
                    io: self.clone(),
                    reservation,
                    intent,
                })
            }
        }
        pub(super) struct NativeHistoryPort<'a> {
            pub(super) io: &'a Arc<WindowsNativeIo>,
            pub(super) lock: &'a InstallerLock,
            pub(super) reservation: &'a FirstInstallReservation,
            pub(super) deadline: &'a Deadline,
        }
        impl FirstHistoryPort for NativeHistoryPort<'_> {
            fn renew(&mut self, intent: &FirstHistoryIntent) -> NativeResult<()> {
                let proof = self.io.admit_support(self.deadline)?;
                self.io.verify_stop_lock(&proof, self.lock, self.deadline)?;
                self.reservation.reverify(self.io, &proof, self.deadline)?;
                intent.validate()?;
                if let Some(old) = self.io.read_first_history_intent(&proof, self.deadline)?
                    && old.selected.next_operation == intent.selected.next_operation
                    && (old.slot != intent.slot
                        || old.selected != intent.selected
                        || old.moved > intent.moved
                        || old.complete && !intent.complete)
                {
                    return Err(NativeError::Foreign);
                }
                Ok(())
            }
            fn persist_intent(&mut self, intent: &FirstHistoryIntent) -> NativeResult<()> {
                let proof = self.io.admit_support(self.deadline)?;
                self.renew(intent)?;
                let state = self.io.publish_record(
                    &proof,
                    self.lock,
                    records::RecordName::FirstInstallHistoryIntent,
                    &intent.encode()?,
                    self.deadline,
                )?;
                if state.native_failure.is_some()
                    || state.state != records::PublicationRecovery::NewPublished
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                Ok(())
            }
            fn observe_pair(
                &mut self,
                intent: &FirstHistoryIntent,
                leaf: &HistoryLeaf,
            ) -> NativeResult<(Option<HistoryObservation>, Option<HistoryObservation>)>
            {
                let proof = self.io.admit_support(self.deadline)?;
                self.renew(intent)?;
                let parent = self
                    .io
                    .first_history_parent(&proof, self.lock, self.deadline)?;
                let context = self.io.context.clone();
                let leaf = leaf.clone();
                let slot = intent.slot;
                let budget = proof.budget(self.io, self.deadline)?;
                self.io
                    .owner
                    .run(Dispatch::Observation, self.deadline, move || {
                        let name = PrivateName::new(&leaf.leaf)?;
                        let source = parent
                            .read_private(
                                &name,
                                &context.security,
                                files::MAX_RECORD_BYTES,
                                &budget,
                            )?
                            .map(|(id, bytes)| (id.into(), bytes));
                        let destination = parent
                            .first_history_slot(slot, false, &context.security, &budget)?
                            .map(|archive| {
                                archive.read_private(
                                    &name,
                                    &context.security,
                                    files::MAX_RECORD_BYTES,
                                    &budget,
                                )
                            })
                            .transpose()?
                            .flatten()
                            .map(|(id, bytes)| (id.into(), bytes));
                        Ok((source, destination))
                    })
            }
            fn move_exact(
                &mut self,
                intent: &FirstHistoryIntent,
                leaf: &HistoryLeaf,
            ) -> NativeResult<()> {
                let proof = self.io.admit_support(self.deadline)?;
                self.renew(intent)?;
                if self
                    .io
                    .read_first_history_intent(&proof, self.deadline)?
                    .as_ref()
                    != Some(intent)
                {
                    return Err(NativeError::Foreign);
                }
                let parent = self
                    .io
                    .first_history_parent(&proof, self.lock, self.deadline)?;
                let context = self.io.context.clone();
                let leaf = leaf.clone();
                let slot = intent.slot;
                let reservation = self.reservation.clone();
                let lease = self.lock.0.clone();
                let budget = proof.budget(self.io, self.deadline)?;
                self.io
                    .owner
                    .run(Dispatch::Mutation, self.deadline, move || {
                        let change = Change::new();
                        change.finish((|| {
                            validate_payload_lock(&context, &lease, &budget)?;
                            reservation.renew_native(&context, &budget)?;
                            let (id, bytes) = parent
                                .read_private(
                                    &PrivateName::new(&leaf.leaf)?,
                                    &context.security,
                                    files::MAX_RECORD_BYTES,
                                    &budget,
                                )?
                                .ok_or(NativeError::Foreign)?;
                            if !leaf.matches(id.into(), &bytes) {
                                return Err(NativeError::Foreign);
                            }
                            change.reached();
                            let archive = parent
                                .first_history_slot(slot, true, &context.security, &budget)?
                                .ok_or(NativeError::Foreign)?;
                            parent.archive_first_history_metadata(
                                &leaf.leaf,
                                id,
                                &bytes,
                                &archive,
                                &context.security,
                                &budget,
                            )?;
                            reservation.renew_native(&context, &budget)
                        })())
                    })
            }
            fn persist_index(&mut self, index: &FirstHistoryIndex) -> NativeResult<()> {
                let proof = self.io.admit_support(self.deadline)?;
                self.io.verify_stop_lock(&proof, self.lock, self.deadline)?;
                self.reservation.reverify(self.io, &proof, self.deadline)?;
                let result = self.io.publish_record(
                    &proof,
                    self.lock,
                    records::RecordName::FirstInstallHistoryIndex,
                    &index.encode()?,
                    self.deadline,
                )?;
                if result.native_failure.is_some()
                    || result.state != records::PublicationRecovery::NewPublished
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                Ok(())
            }
        }
    }
    #[cfg(not(test))]
    pub(crate) use first_history_io::{ArchivedFirstHistory, FirstInstallArchiveResult};

    #[cfg(not(test))]
    mod first_recovery_io {
        use super::super::super::{
            first_install::record::{
                FirstInstallRecord, FirstRecoveryCursor, FirstRecoveryMode, FirstRecoveryRecord,
                HistoryLeaf, RoleTriad,
            },
            payload::{
                inventory::PayloadRole,
                recovery::{FileStamp, OriginalLeaf, OuterContextCorrelation},
            },
            removal::inventory::PartialFirstLocation as Location,
            service::task::{Definition, Logon, RunLevel, SUPERVISOR_ARGUMENT, TASK_NAME},
        };
        use super::*;
        struct FirstPriorSessionDisposed {
            io: Arc<WindowsNativeIo>,
            document: FirstInstallRecord,
        }
        impl FirstPriorSessionDisposed {
            fn reverify(&self, proof: &SupportProof, deadline: &Deadline) -> NativeResult<()> {
                renew_prior_context(&self.io, proof, &self.document, deadline)
            }
        }
        pub(crate) struct FirstRecoveryReservation {
            io: Arc<WindowsNativeIo>,
            reservation: FirstInstallReservation,
            source: HistoryLeaf,
            document: FirstInstallRecord,
            prior: Option<FirstPriorSessionDisposed>,
        }
        impl FirstRecoveryReservation {
            fn reverify(
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
                self.reservation.reverify(io, proof, deadline)?;
                let source = io.first_recovery_source(
                    proof,
                    &self.source,
                    self.document.operation(),
                    deadline,
                )?;
                if !self.source.matches(source.0, &source.1)
                    || FirstInstallRecord::decode(&source.1)? != self.document
                {
                    return Err(NativeError::Foreign);
                }
                match &self.prior {
                    Some(prior) => prior.reverify(proof, deadline),
                    None => renew_prior_context(io, proof, &self.document, deadline),
                }
            }
        }
        /// Keeps the native cause; a denied Task Scheduler connection is never proof of absence.
        fn repair_connect_failure(failure: super::super::task::RepairTaskFailure) -> NativeError {
            match failure {
                super::super::task::RepairTaskFailure::Native(error) => error,
                super::super::task::RepairTaskFailure::AccessDenied => NativeError::Foreign,
            }
        }
        fn renew_prior_context(
            io: &WindowsNativeIo,
            proof: &SupportProof,
            source: &FirstInstallRecord,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let old: OuterContextCorrelation =
                serde_json::from_slice(source.context()).map_err(|_| NativeError::Invalid)?;
            renew_prior_correlation(io, proof, &old, deadline)
        }
        /// Shared F2 disposition for a stored correlation: the same user is required, and a
        /// different logon must have ended (`query_ended_logon`). First-install and history
        /// resumes both use it.
        pub(super) fn renew_prior_correlation(
            io: &WindowsNativeIo,
            proof: &SupportProof,
            old: &OuterContextCorrelation,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            old.same_user(&io.context.target.identity)?;
            if old.matches(&io.context.target.identity).is_ok() {
                return Ok(());
            }
            if old.authentication_id() == io.context.target.identity.authentication_id
                || old.logon_sid() == io.context.target.identity.logon.bytes()
                || LOGON_QUERY_QUARANTINE.get().is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            let context = io.context.clone();
            let owner = io.owner.clone();
            let authentication_id = old.authentication_id();
            let budget = proof.budget(io, deadline)?;
            // Existing bounded worker returns success ONLY for NO_SUCH_LOGON_SESSION; this
            // observation grants file/history recovery, never job zero or a process capability.
            io.owner
                .run(Dispatch::Observation, deadline, move || {
                    query_ended_logon(&context, &owner, authentication_id, &budget)
                })
                .map_err(|_| NativeError::OutcomeUnknown)
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
        fn root(context: &Context, budget: &Deadline) -> NativeResult<Option<Anchor>> {
            Anchor::open(
                context.target.paths.install(),
                &context.security,
                true,
                budget,
            )
        }
        fn parent(
            context: &Context,
            operation: [u8; 16],
            location: Location,
            budget: &Deadline,
        ) -> NativeResult<Option<Anchor>> {
            let Some(root) = root(context, budget)? else {
                return Ok(None);
            };
            if location == Location::Fixed {
                return Ok(Some(root));
            }
            let name = match location {
                Location::Stage => "first-install-stage",
                Location::Backup => "first-install-backups",
                Location::Fixed => return Err(NativeError::Invalid),
            };
            let Some(group) = root.child(name, &context.security, budget)? else {
                return Ok(None);
            };
            group.child(&records::hex(&operation), &context.security, budget)
        }
        fn validate_scaffold(
            context: &Context,
            record: &FirstRecoveryRecord,
            budget: &Deadline,
        ) -> NativeResult<()> {
            let Some(root) = root(context, budget)? else {
                return if record.scaffold[0].is_none() {
                    Ok(())
                } else {
                    Err(NativeError::Foreign)
                };
            };
            if Some(root.identity()?.into()) != record.scaffold[0] {
                return Err(NativeError::Foreign);
            }
            for (base, name) in [(1, "first-install-stage"), (3, "first-install-backups")] {
                let group = root.child(name, &context.security, budget)?;
                if group
                    .as_ref()
                    .map(Anchor::identity)
                    .transpose()?
                    .map(Into::into)
                    != record.scaffold[base]
                {
                    return Err(NativeError::Foreign);
                }
                if let Some(group) = group {
                    let op = group.child(
                        &records::hex(&record.document.operation()),
                        &context.security,
                        budget,
                    )?;
                    if op
                        .as_ref()
                        .map(Anchor::identity)
                        .transpose()?
                        .map(Into::into)
                        != record.scaffold[base + 1]
                    {
                        return Err(NativeError::Foreign);
                    }
                }
            }
            Ok(())
        }
        fn check(
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            reservation: &FirstRecoveryReservation,
            record: &FirstRecoveryRecord,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            record.validate()?;
            reservation.reverify(io, proof, lock, deadline)?;
            if record.source != reservation.source || record.document != reservation.document {
                return Err(NativeError::Foreign);
            }
            if io.read_first_recovery(proof, deadline)?.as_ref() != Some(record) {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
        impl WindowsNativeIo {
            fn first_recovery_source(
                &self,
                proof: &SupportProof,
                expected: &HistoryLeaf,
                operation: [u8; 16],
                deadline: &Deadline,
            ) -> NativeResult<(FileStamp, Vec<u8>)> {
                if let Some(source) = self.read_record(
                    proof,
                    records::RecordName::FirstInstall,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )? {
                    if !expected.matches(source.identity.into(), source.bytes()) {
                        return Err(NativeError::Foreign);
                    }
                    return Ok((source.identity.into(), source.bytes().to_vec()));
                }
                let recovery = self
                    .read_first_recovery(proof, deadline)?
                    .ok_or(NativeError::Foreign)?;
                if recovery.source != *expected
                    || recovery.document.operation() != operation
                    || !matches!(
                        recovery.cursor,
                        FirstRecoveryCursor::RetireIntent | FirstRecoveryCursor::Retired
                    )
                {
                    return Err(NativeError::Foreign);
                }
                self.read_archived_first_source(proof, expected, operation, deadline)
            }
            pub(crate) fn renew_first_recovery(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstRecoveryReservation,
                record: &FirstRecoveryRecord,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                reservation.reverify(self, proof, lock, deadline)?;
                if record.source != reservation.source || record.document != reservation.document {
                    return Err(NativeError::Foreign);
                }
                record.validate()
            }
            pub(crate) fn read_first_recovery(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<Option<FirstRecoveryRecord>> {
                self.read_record(
                    proof,
                    records::RecordName::FirstInstallRecovery,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .map(|r| FirstRecoveryRecord::decode(r.bytes()))
                .transpose()
            }
            fn exclude_other_first_recovery(
                &self,
                proof: &SupportProof,
                source: &FirstInstallRecord,
                mode: FirstRecoveryMode,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                let context = self.context.clone();
                let budget = proof.budget(self, deadline)?;
                let names = self.owner.run(Dispatch::Observation, deadline, move || {
                    let parent = Anchor::open(
                        context.target.paths.installer(),
                        &context.security,
                        true,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?;
                    parent.entry_names(&context.security, &budget)
                })?;
                let stale = matches!(
                    mode,
                    FirstRecoveryMode::Supersede | FirstRecoveryMode::RetireStale
                );
                for name in names {
                    if matches!(
                        name.as_str(),
                        "install.lock"
                            | "first-install.json"
                            | "first-install-recovery.json"
                            | "first-install-history"
                            | "first-install-history-index.json"
                            | "first-install-history-intent.json"
                            | "task-activation.json"
                            | crosspane_installer_core::elevated::journal::RECORD_LEAF
                    ) {
                        continue;
                    }
                    if stale
                        && matches!(
                            name.as_str(),
                            "supervisor.json"
                                | "supervisor-logon.json"
                                | "supervisor-epoch-0.json"
                                | "supervisor-epoch-1.json"
                                | "supervisor-epoch-2.json"
                                | "supervisor-archive-intent.json"
                        )
                    {
                        continue;
                    }
                    return Err(NativeError::OutcomeUnknown);
                }
                if let Some(task) =
                    super::super::activation::TaskActivationRecord::read(self, proof, deadline)?
                    && (task.operation() != source.operation()
                        || (!stale && task.claim().is_some()))
                {
                    return Err(NativeError::Foreign);
                }
                if let Some(intent) = self.read_first_history_intent(proof, deadline)? {
                    if intent.complete{self.verify_first_history(proof,&intent,deadline)?;}else if intent.selected.operation!=source.operation() || intent.selected.source==super::super::super::first_install::record::FirstHistorySource::Removal {return Err(NativeError::OutcomeUnknown);}
                }
                if stale {
                    use super::super::super::service::journal::Journal;
                    for name in [
                        records::RecordName::Supervisor,
                        records::RecordName::SupervisorEpoch(0),
                        records::RecordName::SupervisorEpoch(1),
                        records::RecordName::SupervisorEpoch(2),
                    ] {
                        if let Some(r) =
                            self.read_record(proof, name, files::MAX_RECORD_BYTES, deadline)?
                        {
                            Journal::decode(r.bytes())?;
                        }
                    }
                    super::super::activation::SupervisorLogonRecord::read(self, proof, deadline)?;
                    if let Some(r) = self.read_record(
                        proof,
                        records::RecordName::SupervisorArchiveIntent,
                        files::MAX_RECORD_BYTES,
                        deadline,
                    )? {
                        let intent: super::super::epoch_archive::ArchiveIntent =
                            records::record_data(
                                &records::RecordName::SupervisorArchiveIntent,
                                r.bytes(),
                            )?;
                        intent.require_complete()?;
                    }
                }
                Ok(())
            }
            pub(crate) fn reserve_first_recovery(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                source: &FirstInstallRecord,
                mode: FirstRecoveryMode,
                deadline: &Deadline,
            ) -> NativeResult<FirstRecoveryReservation> {
                self.verify_stop_lock(proof, lock, deadline)?;
                source.validate()?;
                self.exclude_other_first_recovery(proof, source, mode, deadline)?;
                let (identity, bytes) = match self.read_record(
                    proof,
                    records::RecordName::FirstInstall,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )? {
                    Some(selected) => (selected.identity.into(), selected.bytes().to_vec()),
                    None => {
                        let old = self
                            .read_first_recovery(proof, deadline)?
                            .ok_or(NativeError::Foreign)?;
                        if old.document != *source {
                            return Err(NativeError::Foreign);
                        }
                        self.first_recovery_source(
                            proof,
                            &old.source,
                            source.operation(),
                            deadline,
                        )?
                    }
                };
                if FirstInstallRecord::decode(&bytes)? != *source {
                    return Err(NativeError::Foreign);
                }
                renew_prior_context(self, proof, source, deadline)?;
                let reservation = self.acquire_first_reservation(proof, lock, false, deadline)?;
                let value = FirstRecoveryReservation {
                    io: self.clone(),
                    reservation,
                    source: HistoryLeaf::observe("first-install.json".into(), identity, &bytes)?,
                    document: source.clone(),
                    prior: {
                        let old: OuterContextCorrelation = serde_json::from_slice(source.context())
                            .map_err(|_| NativeError::Invalid)?;
                        if old.matches(&self.context.target.identity).is_ok() {
                            None
                        } else {
                            Some(FirstPriorSessionDisposed {
                                io: self.clone(),
                                document: source.clone(),
                            })
                        }
                    },
                };
                value.reverify(self, proof, lock, deadline)?;
                Ok(value)
            }
            fn previous_first_recovery_settled(
                &self,
                proof: &SupportProof,
                old: &FirstRecoveryRecord,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if old.cursor != FirstRecoveryCursor::Retired {
                    return Err(NativeError::OutcomeUnknown);
                }
                let source = self.read_archived_first_source(
                    proof,
                    &old.source,
                    old.document.operation(),
                    deadline,
                )?;
                if !old.source.matches(source.0, &source.1)
                    || FirstInstallRecord::decode(&source.1)? != old.document
                {
                    return Err(NativeError::Foreign);
                }
                Ok(())
            }
            pub(crate) fn select_first_recovery(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstRecoveryReservation,
                mode: FirstRecoveryMode,
                deadline: &Deadline,
            ) -> NativeResult<FirstRecoveryRecord> {
                reservation.reverify(self, proof, lock, deadline)?;
                if let Some(old) = self.read_first_recovery(proof, deadline)? {
                    if old.source != reservation.source || old.document != reservation.document {
                        self.previous_first_recovery_settled(proof, &old, deadline)?;
                    } else {
                        if old.mode != mode {
                            if mode == FirstRecoveryMode::RetireStale {
                                return old.select_retirement();
                            }
                            if mode == FirstRecoveryMode::Remove {
                                return old.select_removal();
                            }
                            return Err(NativeError::Foreign);
                        }
                        if old.pending_scaffold || old.pending.iter().any(|p| *p) {
                            return old.retry_retained();
                        }
                        return Ok(old);
                    }
                }
                // A new archive-bound recovery needs a vacant history slot before any effect. An
                // active intent for this same operation already owns its slot.
                if matches!(
                    mode,
                    FirstRecoveryMode::Rollback
                        | FirstRecoveryMode::Remove
                        | FirstRecoveryMode::RetireStale
                ) {
                    let index = self.read_first_history_index(proof, deadline)?;
                    let active = self.read_first_history_intent(proof, deadline)?;
                    self.validate_first_history_slots(proof, &index, active.as_ref(), deadline)?;
                    let owned = active.as_ref().is_some_and(|intent| {
                        intent.selected.operation == reservation.document.operation()
                    });
                    if !owned {
                        index.vacant()?;
                    }
                }
                let mut record = FirstRecoveryRecord::new(
                    reservation.source.clone(),
                    reservation.document.clone(),
                    mode,
                )?;
                let context = self.context.clone();
                let operation = record.document.operation();
                let budget = proof.budget(self, deadline)?;
                // Only Rollback and Remove record a stray stage leaf, and only for a first install
                // cut at StageIntent(role) before that role's staged identity was persisted.
                let stray_role = match mode {
                    FirstRecoveryMode::Rollback | FirstRecoveryMode::Remove => {
                        record.document.stray_stage_role()
                    }
                    _ => None,
                };
                let (scaffold, stray) =
                    self.owner.run(Dispatch::Observation, deadline, move || {
                        let mut ids: [Option<FileStamp>; 5] = [None; 5];
                        if let Some(root) = root(&context, &budget)? {
                            ids[0] = Some(root.identity()?.into());
                            for (base, name) in
                                [(1, "first-install-stage"), (3, "first-install-backups")]
                            {
                                if let Some(group) = root.child(name, &context.security, &budget)? {
                                    ids[base] = Some(group.identity()?.into());
                                    if let Some(op) = group.child(
                                        &records::hex(&operation),
                                        &context.security,
                                        &budget,
                                    )? {
                                        ids[base + 1] = Some(op.identity()?.into());
                                    }
                                }
                            }
                        }
                        // The leaf must sit in the operation's own stage directory, whose FileId is
                        // scaffold[2]. It must be a regular private file; anything else fails closed.
                        let stray = match stray_role {
                            None => None,
                            Some(role) => {
                                match parent(&context, operation, Location::Stage, &budget)? {
                                    None => None,
                                    Some(stage) => {
                                        if Some(FileStamp::from(stage.identity()?)) != ids[2] {
                                            return Err(NativeError::Foreign);
                                        }
                                        match stage.open_file_metadata(
                                            &files::PrivateName::new(role.leaf())?,
                                            &context.security,
                                            &budget,
                                        ) {
                                            Ok((_, identity)) => {
                                                Some((role, FileStamp::from(identity)))
                                            }
                                            Err(NativeError::Missing) => None,
                                            Err(error) => return Err(error),
                                        }
                                    }
                                }
                            }
                        };
                        Ok((ids, stray))
                    })?;
                record.scaffold = scaffold;
                record.stray_stage = stray;
                record.validate()?;
                Ok(record)
            }
            pub(crate) fn publish_first_recovery(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstRecoveryReservation,
                record: &FirstRecoveryRecord,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                record.validate()?;
                reservation.reverify(self, proof, lock, deadline)?;
                if record.source != reservation.source || record.document != reservation.document {
                    return Err(NativeError::Foreign);
                }
                if let Some(old) = self.read_first_recovery(proof, deadline)? {
                    if old.source == record.source && old.document == record.document {
                        record.follows(&old)?;
                    } else {
                        self.previous_first_recovery_settled(proof, &old, deadline)?;
                        if record.cursor != FirstRecoveryCursor::Selected || record.pass != 0 {
                            return Err(NativeError::Foreign);
                        }
                    }
                }
                let published = self.publish_record(
                    proof,
                    lock,
                    records::RecordName::FirstInstallRecovery,
                    &record.encode()?,
                    deadline,
                )?;
                if published.native_failure.is_some()
                    || published.state != records::PublicationRecovery::NewPublished
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                Ok(())
            }
            pub(crate) fn first_recovery_task_absent(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstRecoveryReservation,
                record: &FirstRecoveryRecord,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                check(self, proof, lock, reservation, record, deadline)?;
                let context = self.context.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let validate = || context.validate(&budget);
                    let scheduler = super::super::task::Scheduler::connect_repair(&validate)
                        .map_err(repair_connect_failure)?;
                    let (_, current) =
                        scheduler.inspect_removal(&definition(&context), &validate)?;
                    Ok(current.is_none())
                })
            }
            pub(crate) fn delete_first_recovery_task(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstRecoveryReservation,
                record: &FirstRecoveryRecord,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                check(self, proof, lock, reservation, record, deadline)?;
                if record.cursor != FirstRecoveryCursor::TaskDeleteIntent {
                    return Err(NativeError::Foreign);
                }
                let context = self.context.clone();
                let lease = lock.0.clone();
                let held = reservation.reservation.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let change = Change::new();
                    change.finish((|| {
                        validate_payload_lock(&context, &lease, &budget)?;
                        held.renew_native(&context, &budget)?;
                        let validate = || {
                            context.validate(&budget)?;
                            held.renew_native(&context, &budget)
                        };
                        let scheduler = super::super::task::Scheduler::connect_repair(&validate)
                            .map_err(repair_connect_failure)?;
                        let desired = definition(&context);
                        let (xml, _) = scheduler.inspect_removal(&desired, &validate)?;
                        scheduler.delete_first_partial(&desired, &xml, &validate, &|| {
                            change.reached()
                        })?;
                        held.renew_native(&context, &budget)
                    })())
                })
            }
            #[allow(clippy::too_many_arguments)]
            pub(crate) fn observe_first_recovery_role(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstRecoveryReservation,
                record: &FirstRecoveryRecord,
                role: PayloadRole,
                location: Location,
                deadline: &Deadline,
            ) -> NativeResult<Option<FileStamp>> {
                check(self, proof, lock, reservation, record, deadline)?;
                let context = self.context.clone();
                let operation = record.document.operation();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let Some(parent) = parent(&context, operation, location, &budget)? else {
                        return Ok(None);
                    };
                    Ok(parent
                        .opaque(role.leaf(), false, &context.security, &budget)?
                        .map(|leaf| leaf.identity.into()))
                })
            }
            pub(crate) fn first_recovery_triad(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstRecoveryReservation,
                record: &FirstRecoveryRecord,
                role: PayloadRole,
                deadline: &Deadline,
            ) -> NativeResult<RoleTriad> {
                Ok(RoleTriad {
                    fixed: self.observe_first_recovery_role(
                        proof,
                        lock,
                        reservation,
                        record,
                        role,
                        Location::Fixed,
                        deadline,
                    )?,
                    stage: self.observe_first_recovery_role(
                        proof,
                        lock,
                        reservation,
                        record,
                        role,
                        Location::Stage,
                        deadline,
                    )?,
                    backup: self.observe_first_recovery_role(
                        proof,
                        lock,
                        reservation,
                        record,
                        role,
                        Location::Backup,
                        deadline,
                    )?,
                })
            }
            #[allow(clippy::too_many_arguments)]
            pub(crate) fn move_first_recovery_role(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstRecoveryReservation,
                record: &FirstRecoveryRecord,
                role: PayloadRole,
                restore: bool,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                check(self, proof, lock, reservation, record, deadline)?;
                let index = 3 - role as u8;
                if record.mode != FirstRecoveryMode::Rollback
                    || record.cursor
                        != (FirstRecoveryCursor::Role {
                            index,
                            step: if restore { 2 } else { 0 },
                        })
                    || !self.first_recovery_task_absent(
                        proof,
                        lock,
                        reservation,
                        record,
                        deadline,
                    )?
                {
                    return Err(NativeError::Foreign);
                }
                let from = if restore {
                    Location::Backup
                } else {
                    Location::Fixed
                };
                let to = if restore {
                    Location::Fixed
                } else {
                    Location::Stage
                };
                let selected = record.document.role(role)?;
                let expected = if restore {
                    match selected.original {
                        OriginalLeaf::Present(id) => id,
                        _ => return Err(NativeError::Foreign),
                    }
                } else {
                    selected
                        .staged
                        .as_ref()
                        .ok_or(NativeError::Foreign)?
                        .identity
                };
                if self.self_image(proof, deadline)?.identity() == epoch_identity(expected) {
                    return Err(NativeError::Unavailable);
                }
                let context = self.context.clone();
                let lease = lock.0.clone();
                let held = reservation.reservation.clone();
                let record = record.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let change = Change::new();
                    change.finish((|| {
                        validate_payload_lock(&context, &lease, &budget)?;
                        held.renew_native(&context, &budget)?;
                        validate_scaffold(&context, &record, &budget)?;
                        let source = parent(&context, record.document.operation(), from, &budget)?
                            .ok_or(NativeError::Foreign)?;
                        let destination =
                            parent(&context, record.document.operation(), to, &budget)?
                                .ok_or(NativeError::Foreign)?;
                        if destination
                            .opaque(role.leaf(), false, &context.security, &budget)?
                            .is_some()
                        {
                            return Err(NativeError::Foreign);
                        }
                        let object = source
                            .opaque(role.leaf(), false, &context.security, &budget)?
                            .ok_or(NativeError::Foreign)?;
                        if object.identity != epoch_identity(expected) {
                            return Err(NativeError::Foreign);
                        }
                        budget.check()?;
                        change.reached();
                        source.move_opaque(
                            object,
                            &destination,
                            role.leaf(),
                            &context.security,
                            &budget,
                        )?;
                        if source
                            .opaque(role.leaf(), false, &context.security, &budget)?
                            .is_some()
                            || destination
                                .opaque(role.leaf(), false, &context.security, &budget)?
                                .is_none_or(|o| o.identity != epoch_identity(expected))
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        held.renew_native(&context, &budget)
                    })())
                })
            }
            #[allow(clippy::too_many_arguments)]
            pub(crate) fn delete_first_recovery_role(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstRecoveryReservation,
                record: &FirstRecoveryRecord,
                role: PayloadRole,
                location: Location,
                expected: FileStamp,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                check(self, proof, lock, reservation, record, deadline)?;
                let selected = record.document.role(role)?;
                let identity = match location {
                    Location::Fixed => selected
                        .published
                        .as_ref()
                        .or(selected.staged.as_ref())
                        .map(|i| i.identity),
                    // The persisted staged identity, or the recorded stray leaf for this role only.
                    Location::Stage => record.stage_identity(role)?,
                    Location::Backup => match selected.original {
                        OriginalLeaf::Present(id) => Some(id),
                        _ => None,
                    },
                };
                let step = match location {
                    Location::Fixed => 0,
                    Location::Stage => 2,
                    Location::Backup => 4,
                };
                let admitted = match record.mode {
                    FirstRecoveryMode::Remove => {
                        record.cursor
                            == FirstRecoveryCursor::Role {
                                index: 3 - role as u8,
                                step,
                            }
                    }
                    FirstRecoveryMode::Rollback => {
                        location == Location::Stage
                            && record.cursor
                                == FirstRecoveryCursor::Role {
                                    index: 3 - role as u8,
                                    step: 4,
                                }
                    }
                    _ => false,
                };
                if !admitted
                    || identity != Some(expected)
                    || !self.first_recovery_task_absent(
                        proof,
                        lock,
                        reservation,
                        record,
                        deadline,
                    )?
                {
                    return Err(NativeError::Foreign);
                }
                if self.self_image(proof, deadline)?.identity() == epoch_identity(expected) {
                    return Ok(false);
                }
                let context = self.context.clone();
                let lease = lock.0.clone();
                let held = reservation.reservation.clone();
                let record = record.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let change = Change::new();
                    change.finish((|| {
                        validate_payload_lock(&context, &lease, &budget)?;
                        held.renew_native(&context, &budget)?;
                        validate_scaffold(&context, &record, &budget)?;
                        let Some(parent) =
                            parent(&context, record.document.operation(), location, &budget)?
                        else {
                            return Ok(true);
                        };
                        let result = parent.delete_first_recovery_leaf(
                            role.leaf(),
                            epoch_identity(expected),
                            &context.security,
                            &budget,
                            &|| change.reached(),
                        )?;
                        held.renew_native(&context, &budget)?;
                        Ok(result)
                    })())
                })
            }
            pub(crate) fn settle_first_recovery_scaffold(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstRecoveryReservation,
                record: &FirstRecoveryRecord,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                check(self, proof, lock, reservation, record, deadline)?;
                if record.cursor != FirstRecoveryCursor::CleanupIntent
                    || !matches!(
                        record.mode,
                        FirstRecoveryMode::Rollback | FirstRecoveryMode::Remove
                    )
                    || !self.first_recovery_task_absent(
                        proof,
                        lock,
                        reservation,
                        record,
                        deadline,
                    )?
                {
                    return Err(NativeError::Foreign);
                }
                let context = self.context.clone();
                let lease = lock.0.clone();
                let held = reservation.reservation.clone();
                let record = record.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let change = Change::new();
                    change.finish((|| {
                        validate_payload_lock(&context, &lease, &budget)?;
                        held.renew_native(&context, &budget)?;
                        // Each retained directory is closed before its exact parent performs ordinary deletion.
                        let Some(root) = root(&context, &budget)? else {
                            return Ok(record.mode == FirstRecoveryMode::Remove
                                || record.scaffold[0].is_none());
                        };
                        if Some(root.identity()?.into()) != record.scaffold[0] {
                            return Err(NativeError::Foreign);
                        }
                        let mut complete = true;
                        for (base, name) in
                            [(1, "first-install-stage"), (3, "first-install-backups")]
                        {
                            if let Some(group) = root.child(name, &context.security, &budget)? {
                                if Some(group.identity()?.into()) != record.scaffold[base] {
                                    complete = false;
                                    continue;
                                }
                                let op = records::hex(&record.document.operation());
                                if let Some(child) = group.child(&op, &context.security, &budget)? {
                                    let id = child.identity()?;
                                    drop(child);
                                    if Some(id.into()) != record.scaffold[base + 1] {
                                        complete = false;
                                        continue;
                                    }
                                    complete &= group.delete_first_recovery_leaf(
                                        &op,
                                        id,
                                        &context.security,
                                        &budget,
                                        &|| change.reached(),
                                    )?;
                                }
                                let id = group.identity()?;
                                drop(group);
                                complete &= root.delete_first_recovery_leaf(
                                    name,
                                    id,
                                    &context.security,
                                    &budget,
                                    &|| change.reached(),
                                )?;
                            }
                        }
                        if record.mode == FirstRecoveryMode::Remove {
                            let id = root.identity()?;
                            drop(root);
                            let programs = Anchor::open(
                                &format!("{}\\Programs", context.target.paths.local()),
                                &context.security,
                                false,
                                &budget,
                            )?
                            .ok_or(NativeError::Missing)?;
                            complete &= programs.delete_first_recovery_leaf(
                                "Crosspane",
                                id,
                                &context.security,
                                &budget,
                                &|| change.reached(),
                            )?;
                        }
                        held.renew_native(&context, &budget)?;
                        Ok(complete)
                    })())
                })
            }
            pub(crate) fn retire_first_recovery(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstRecoveryReservation,
                record: &FirstRecoveryRecord,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                check(self, proof, lock, reservation, record, deadline)?;
                self.archive_first_recovery_history(
                    proof,
                    lock,
                    &reservation.reservation,
                    record,
                    deadline,
                )
            }
            pub(super) fn verify_recovered_first_files(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                reservation: &FirstInstallReservation,
                record: &FirstRecoveryRecord,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                self.verify_stop_lock(proof, lock, deadline)?;
                reservation.reverify(self, proof, deadline)?;
                let context = self.context.clone();
                let budget = proof.budget(self, deadline)?;
                let record = record.clone();
                self.owner.run(Dispatch::Observation, deadline, move || {
                    if record.mode == FirstRecoveryMode::Remove {
                        return if root(&context, &budget)?.is_none() {
                            Ok(())
                        } else {
                            Err(NativeError::Foreign)
                        };
                    }
                    for role in PayloadRole::ALL {
                        for location in [Location::Stage, Location::Backup] {
                            if let Some(parent) =
                                parent(&context, record.document.operation(), location, &budget)?
                                && parent
                                    .opaque(role.leaf(), false, &context.security, &budget)?
                                    .is_some()
                            {
                                return Err(NativeError::Foreign);
                            }
                        }
                        let observed = parent(
                            &context,
                            record.document.operation(),
                            Location::Fixed,
                            &budget,
                        )?
                        .map(|p| p.opaque(role.leaf(), false, &context.security, &budget))
                        .transpose()?
                        .flatten()
                        .map(|o| o.identity.into());
                        match record.document.role(role)?.original {
                            OriginalLeaf::Present(id) if observed == Some(id) => {}
                            OriginalLeaf::Missing if observed.is_none() => {}
                            OriginalLeaf::Unobserved => {}
                            _ => return Err(NativeError::Foreign),
                        }
                    }
                    Ok(())
                })
            }
            pub(crate) fn verify_first_supersession_files(
                &self,
                proof: &SupportProof,
                source: &FirstInstallRecord,
                agent: &AgentObservation,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                source.validate()?;
                agent.revalidate(self, proof, deadline)?;
                let context = self.context.clone();
                let budget = proof.budget(self, deadline)?;
                let source = source.clone();
                let agent_id = self.agent_identity(agent, proof, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let root = root(&context, &budget)?.ok_or(NativeError::Missing)?;
                    for role in PayloadRole::ALL {
                        let selected = source.role(role)?;
                        let expected = selected.published.as_ref().ok_or(NativeError::Foreign)?;
                        let image = root.open_image(
                            role.leaf(),
                            true,
                            &expected.facts.version,
                            &context.security,
                            &budget,
                        )?;
                        if image.identity != epoch_identity(expected.identity)
                            || image.facts != selected.approved
                            || (role == PayloadRole::Agent && image.identity != agent_id)
                        {
                            return Err(NativeError::Foreign);
                        }
                    }
                    Ok(())
                })
            }
            pub(crate) fn select_live_first_recovery(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                source: &FirstInstallRecord,
                deadline: &Deadline,
            ) -> NativeResult<FirstRecoveryRecord> {
                self.verify_stop_lock(proof, lock, deadline)?;
                self.exclude_other_first_recovery(
                    proof,
                    source,
                    FirstRecoveryMode::Supersede,
                    deadline,
                )?;
                let actual = self
                    .read_record(
                        proof,
                        records::RecordName::FirstInstall,
                        files::MAX_RECORD_BYTES,
                        deadline,
                    )?
                    .ok_or(NativeError::Foreign)?;
                if FirstInstallRecord::decode(actual.bytes())? != *source {
                    return Err(NativeError::Foreign);
                }
                let selected = HistoryLeaf::observe(
                    "first-install.json".into(),
                    actual.identity.into(),
                    actual.bytes(),
                )?;
                if let Some(old) = self.read_first_recovery(proof, deadline)? {
                    if old.source != selected
                        || old.document != *source
                        || old.mode != FirstRecoveryMode::Supersede
                    {
                        return Err(NativeError::Foreign);
                    }
                    return Ok(old);
                }
                FirstRecoveryRecord::new(selected, source.clone(), FirstRecoveryMode::Supersede)
            }
            pub(crate) fn publish_live_first_recovery(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                record: &FirstRecoveryRecord,
                live: &super::super::supervisor_owner::LiveFirstSupersession,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                self.verify_stop_lock(proof, lock, deadline)?;
                record.validate()?;
                if record.mode != FirstRecoveryMode::Supersede
                    || record.cursor != FirstRecoveryCursor::Superseded
                {
                    return Err(NativeError::Foreign);
                }
                live.reverify(self, deadline)?;
                let actual = self
                    .read_record(
                        proof,
                        records::RecordName::FirstInstall,
                        files::MAX_RECORD_BYTES,
                        deadline,
                    )?
                    .ok_or(NativeError::Foreign)?;
                if !record
                    .source
                    .matches(actual.identity.into(), actual.bytes())
                    || FirstInstallRecord::decode(actual.bytes())? != record.document
                {
                    return Err(NativeError::Foreign);
                }
                if let Some(old) = self.read_first_recovery(proof, deadline)? {
                    record.follows(&old)?;
                    if old == *record {
                        return Ok(());
                    }
                }
                let published = self.publish_record(
                    proof,
                    lock,
                    records::RecordName::FirstInstallRecovery,
                    &record.encode()?,
                    deadline,
                )?;
                if published.native_failure.is_some()
                    || published.state != records::PublicationRecovery::NewPublished
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                live.reverify(self, deadline)
            }
            pub(crate) fn first_source_settled(
                &self,
                proof: &SupportProof,
                source: &FirstInstallRecord,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                let Some(recovery) = self.read_first_recovery(proof, deadline)? else {
                    return Ok(false);
                };
                if recovery.mode != FirstRecoveryMode::Supersede
                    || recovery.cursor != FirstRecoveryCursor::Superseded
                    || recovery.document != *source
                {
                    return Ok(false);
                }
                let actual = self
                    .read_record(
                        proof,
                        records::RecordName::FirstInstall,
                        files::MAX_RECORD_BYTES,
                        deadline,
                    )?
                    .ok_or(NativeError::Foreign)?;
                Ok(recovery
                    .source
                    .matches(actual.identity.into(), actual.bytes()))
            }
        }
    }
    #[cfg(not(test))]
    pub(crate) use first_recovery_io::FirstRecoveryReservation;
}
