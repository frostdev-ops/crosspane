//! Operation-bound task and supervisor decisions. Model facts never grant native authority.
//! Genuine fixed-role inventory supplies image approval. Native A3 activation and whole-tree
//! completion remain Unsupported until WP-W4.1a4b; image release is not tree completion.

#[path = "service/journal.rs"]
pub mod journal;
#[path = "service/supervisor.rs"]
pub mod supervisor;
#[path = "service/task.rs"]
pub mod task;

use super::native_io::{NativeError, NativeResult};

/// Only genuine fixed-role approval can construct the production image capability.
struct TrustedImages {
    _sealed: (),
    #[cfg(all(windows, not(test)))]
    _native: std::sync::Arc<NativeImages>,
}
impl TrustedImages {
    fn current() -> NativeResult<Self> {
        #[cfg(all(windows, not(test)))]
        {
            Self::current_native()
        }
        #[cfg(any(not(windows), test))]
        {
            // The unchanged A3 fake roots never inspect native folders or gain launch authority.
            Err(NativeError::Unsupported)
        }
    }
    #[cfg(test)]
    fn fixture() -> Self {
        Self { _sealed: () }
    }
}

#[cfg(all(windows, not(test)))]
struct NativeImages {
    io: std::sync::Arc<super::native_io::WindowsNativeIo>,
    _root: super::native_io::PayloadRoot,
    // A self-pin is an own opened module, not a hash adopted from the stage catalog.
    module: super::native_io::SelfImagePin,
    installer: super::native_io::OpenedPe,
    agent: super::native_io::OpenedPe,
    ui: super::native_io::OpenedPe,
}

#[cfg(all(windows, not(test)))]
impl TrustedImages {
    fn current_native() -> NativeResult<Self> {
        use super::native_io::{Cancellation, Deadline, MonotonicClock, WindowsNativeIo};
        use super::payload::inventory::{ApprovedInventory, ApprovedPe, PayloadRole};
        use std::sync::Arc;

        // Missing or invalid build-time provenance refuses BEFORE any native context is opened.
        let inventory = ApprovedInventory::embedded()?;
        let clock: Arc<dyn super::native_io::Clock> = Arc::new(MonotonicClock::default());
        let deadline = Deadline::new(30_000, clock.clone(), Cancellation::default())?;
        let io = Arc::new(WindowsNativeIo::current(clock.clone(), &deadline)?);
        let proof = io.admit_support(&deadline)?;
        let module = io.self_image(&proof, &deadline)?;
        let installer_pin = ApprovedPe::own_image(&module)?;
        let proof = io.admit_support(&deadline)?;
        let lock = io.acquire_installer_lock(&proof, &deadline)?;
        let proof = io.admit_support(&deadline)?;
        let root = io.payload_root(&proof, &lock, &deadline)?;
        let proof = io.admit_support(&deadline)?;
        let installer = root.open_approved(
            &io,
            &proof,
            PayloadRole::Installer,
            &installer_pin,
            &deadline,
        )?;
        let proof = io.admit_support(&deadline)?;
        let agent = root.open_approved(
            &io,
            &proof,
            PayloadRole::Agent,
            inventory.role(PayloadRole::Agent)?,
            &deadline,
        )?;
        let proof = io.admit_support(&deadline)?;
        let ui = root.open_approved(
            &io,
            &proof,
            PayloadRole::Ui,
            inventory.role(PayloadRole::Ui)?,
            &deadline,
        )?;
        // Read-only root/image pins outlive this constructor; the operation lock MUST NOT.
        drop(lock);
        let native = Arc::new(NativeImages {
            io,
            _root: root,
            module,
            installer,
            agent,
            ui,
        });
        native.reverify(&deadline)?;
        Ok(Self {
            _sealed: (),
            _native: native,
        })
    }
}

#[cfg(all(windows, not(test)))]
impl NativeImages {
    fn reverify(&self, deadline: &super::native_io::Deadline) -> NativeResult<()> {
        let proof = self.io.admit_support(deadline)?;
        self.module.reverify(&self.io, &proof, deadline)?;
        for image in [&self.installer, &self.agent, &self.ui] {
            let proof = self.io.admit_support(deadline)?;
            image.reverify(&self.io, &proof, deadline)?;
        }
        deadline.check()
    }
}

/// Complete old-origin/tree settlement, not an image-open or record observation.
/// WP-W4.1a4b must supply the production native factory; only fakes can mint this in a4.
pub(crate) struct UpgradeStopProof {
    operation: [u8; 16],
    original_instance: Option<u64>,
    _sealed: (),
}
impl UpgradeStopProof {
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn original_instance(&self) -> Option<u64> {
        self.original_instance
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the source-included windows_payload/windows_upgrade integration roots.
    pub(crate) fn fixture(
        operation: [u8; 16],
        original_instance: Option<u64>,
    ) -> NativeResult<Self> {
        if operation == [0; 16] || original_instance == Some(0) {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            operation,
            original_instance,
            _sealed: (),
        })
    }
}

/// Exact new image and authenticated u64 instance evidence. Never deserialized as authority.
pub(crate) struct NewInstanceEvidence {
    operation: [u8; 16],
    instance: u64,
    image_identity: super::native_io::files::FileIdentity,
    _sealed: (),
}
impl NewInstanceEvidence {
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn instance(&self) -> u64 {
        self.instance
    }
    pub(crate) fn image_identity(&self) -> super::native_io::files::FileIdentity {
        self.image_identity
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the source-included windows_payload/windows_upgrade integration roots.
    pub(crate) fn fixture(
        operation: [u8; 16],
        instance: u64,
        image_identity: super::native_io::files::FileIdentity,
    ) -> NativeResult<Self> {
        if operation == [0; 16]
            || instance == 0
            || image_identity.volume == 0
            || image_identity.file == [0; 16]
        {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            operation,
            instance,
            image_identity,
            _sealed: (),
        })
    }
}

/// No task/child/tree capability is fabricated while native activation is split to a4b.
#[cfg(any(windows, test))]
pub(crate) struct NativeUpgradePort {
    _sealed: (),
}
#[cfg(any(windows, test))]
impl NativeUpgradePort {
    pub(crate) fn new() -> Self {
        Self { _sealed: () }
    }
}
#[cfg(any(windows, test))]
impl super::payload::health::ServicePort for NativeUpgradePort {
    fn stop_for_replace(&mut self, _operation: [u8; 16]) -> NativeResult<UpgradeStopProof> {
        // Leaf exclusivity cannot stand in for the missing original supervisor/job proof.
        Err(NativeError::Unsupported)
    }
    fn start_once(
        &mut self,
        _operation: [u8; 16],
        _payload: &super::payload::health::VerifiedPayload,
    ) -> NativeResult<NewInstanceEvidence> {
        Err(NativeError::Unsupported)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorExit {
    NormalQuit,
    InstallerStopped,
}

/// Windows-only early entry; it cannot start a GUI or manufacture image approval.
#[cfg(windows)]
pub fn supervisor_entry() -> NativeResult<SupervisorExit> {
    let trusted = TrustedImages::current()?;
    supervisor::native_entry(&trusted)
}

/// A4 must supply genuine fixed installer/agent role and PE/hash approval before this can start.
pub fn start_supported() -> NativeResult<()> {
    let trusted = TrustedImages::current()?;
    task::native_start(&trusted)
}

/// This fixed mode accepts no additional argv and confers no execution authority.
pub fn supervisor_mode(arguments: &[std::ffi::OsString]) -> NativeResult<bool> {
    if !arguments
        .iter()
        .any(|argument| argument == task::SUPERVISOR_ARGUMENT)
    {
        return Ok(false);
    }
    if arguments.len() != 1 {
        return Err(NativeError::Invalid);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_start_has_no_current_byte_or_receipt_approval_fallback() {
        let _fixture = TrustedImages::fixture();
        assert_eq!(start_supported(), Err(NativeError::Unsupported));
    }
}
