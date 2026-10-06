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
#[cfg(windows)]
#[allow(unused_imports)]
// A4 consumes completion fields; terminal ACK presently only rechecks origin.
pub(crate) use adapter::ExitObservation;
#[cfg(all(windows, test))]
#[allow(unused_imports)]
// Source-included probe exports; library unit tests do not invoke them.
pub(crate) use adapter::scratch::{FixtureLock, ScratchFixture, scratch_current};
#[cfg(windows)]
#[allow(unused_imports)]
// A4b/A5 consumers retain these sealed native return capabilities.
pub(crate) use adapter::{ImageReleased, OpaqueLeaf, PrunedGeneration};
#[cfg(windows)]
pub use adapter::{InstallerLock, SupportProof, WindowsNativeIo, WindowsTarget};
#[cfg(windows)]
pub(crate) use adapter::{OpenedPe, PayloadRoot, PruneOutcome, SelfImagePin, StagedPe};

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
                        let receipt = observed
                            .runtime
                            .read_private(
                                &PrivateName::new("last_exit.json")?,
                                &context.security,
                                crate::agent_contract::MAX_RESPONSE_BYTES,
                                &budget,
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
