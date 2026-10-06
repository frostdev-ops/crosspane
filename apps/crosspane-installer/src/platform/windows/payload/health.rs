//! Actual health requires exact freshly admitted image and a new authenticated instance.
use super::super::native_io::{NativeError, NativeResult, files::FileIdentity};
use super::inventory::{ApprovedPe, PayloadRole};
#[cfg(any(windows, test))]
pub(crate) trait ServicePort {
    // A4b/A5 supplies genuine native old-owner completion before the install entry is activated.
    #[allow(dead_code)]
    fn stop_for_replace(
        &mut self,
        operation: [u8; 16],
    ) -> NativeResult<super::super::service::UpgradeStopProof>;
    fn start_once(
        &mut self,
        operation: [u8; 16],
        payload: &VerifiedPayload,
    ) -> NativeResult<super::super::service::NewInstanceEvidence>;
    /// A4b must supply fresh retained-owner completion. This is NEVER a repeated stop RPC.
    fn recover_stop(
        &mut self,
        _record: &super::recovery::OperationRecord,
    ) -> NativeResult<super::super::service::UpgradeStopProof> {
        Err(NativeError::Unsupported)
    }
    /// A4b must supply a fresh actual new-instance observation. This is NEVER a repeated start.
    fn recover_started(
        &mut self,
        _record: &super::recovery::OperationRecord,
        _payload: &VerifiedPayload,
    ) -> NativeResult<Option<super::super::service::NewInstanceEvidence>> {
        Err(NativeError::Unsupported)
    }
}
pub(crate) struct VerifiedPayload {
    // A4b native launch/health consumes the exact verified operation and role pins.
    #[allow(dead_code)]
    operation: [u8; 16],
    // A4b native launch/health consumes the exact verified operation and role pins.
    #[allow(dead_code)]
    pins: Vec<ApprovedPe>,
    agent: FileIdentity,
    #[cfg(windows)]
    images: Vec<super::super::native_io::OpenedPe>,
}
impl VerifiedPayload {
    // A4b native launch/health consumes the exact verified operation and role pins.
    #[allow(dead_code)]
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    // A4b native launch/health consumes the exact verified operation and role pins.
    #[allow(dead_code)]
    pub(crate) fn pin(&self, role: PayloadRole) -> NativeResult<&ApprovedPe> {
        self.pins
            .iter()
            .find(|pin| pin.role() == role)
            .ok_or(NativeError::Unsupported)
    }
    pub(crate) fn agent_identity(&self) -> NativeResult<FileIdentity> {
        Ok(self.agent)
    }
    #[cfg(windows)]
    pub(crate) fn reverify(
        &self,
        io: &super::super::native_io::WindowsNativeIo,
        proof: &super::super::native_io::SupportProof,
        deadline: &super::super::native_io::Deadline,
    ) -> NativeResult<()> {
        if self.images.len() != 4 {
            return Err(NativeError::Unsupported);
        }
        for image in &self.images {
            image.reverify(io, proof, deadline)?;
        }
        Ok(())
    }
    #[cfg(windows)]
    pub(crate) fn open(
        operation: [u8; 16],
        io: &super::super::native_io::WindowsNativeIo,
        proof: &super::super::native_io::SupportProof,
        root: &super::super::native_io::PayloadRoot,
        inventory: &super::inventory::ApprovedInventory,
        installer: &ApprovedPe,
        deadline: &super::super::native_io::Deadline,
    ) -> NativeResult<Self> {
        if operation == [0; 16] {
            return Err(NativeError::Invalid);
        }
        let mut images = Vec::new();
        let mut pins = Vec::new();
        let mut agent = None;
        for role in PayloadRole::ALL {
            let pin = if role == PayloadRole::Installer {
                installer
            } else {
                inventory.role(role)?
            };
            let image = root.open_approved(io, proof, role, pin, deadline)?;
            if role == PayloadRole::Agent {
                agent = Some(image.identity())
            }
            pins.push(pin.clone());
            images.push(image);
        }
        Ok(Self {
            operation,
            images,
            pins,
            agent: agent.ok_or(NativeError::Unsupported)?,
        })
    }
    /// Inert test-only value to prove the production service port refuses before reading pins.
    #[cfg(test)]
    // Used by source-included integration tests; the library test root does not call this fixture.
    #[allow(dead_code)]
    pub(crate) fn fixture(operation: [u8; 16]) -> NativeResult<Self> {
        if operation == [0; 16] {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            operation,
            pins: Vec::new(),
            agent: FileIdentity {
                volume: 0,
                file: [0; 16],
            },
            #[cfg(windows)]
            images: Vec::new(),
        })
    }
    #[cfg(test)]
    // Used by source-included integration tests; the library test root does not call this fixture.
    #[allow(dead_code)]
    pub(crate) fn fixture_with_pins(
        operation: [u8; 16],
        pins: Vec<ApprovedPe>,
        agent: FileIdentity,
    ) -> NativeResult<Self> {
        if operation == [0; 16]
            || agent.volume == 0
            || agent.file == [0; 16]
            || pins.len() != 4
            || PayloadRole::ALL
                .iter()
                .any(|role| pins.iter().filter(|p| p.role() == *role).count() != 1)
        {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            operation,
            pins,
            agent,
            #[cfg(windows)]
            images: Vec::new(),
        })
    }
}
/// Typed service evidence is checked against freshly verified exact payload facts.
#[cfg(any(windows, test))]
// The abstract upgrade fakes exercise this typed health gate; A4b native service evidence is deferred.
#[allow(dead_code)]
pub(crate) fn check_instance(
    stop: &super::super::service::UpgradeStopProof,
    new: &super::super::service::NewInstanceEvidence,
    payload: &VerifiedPayload,
) -> NativeResult<u64> {
    if stop.operation() != new.operation()
        || new.operation() != payload.operation()
        || stop.original_instance() == Some(new.instance())
        || new.instance() == 0
        || new.image_identity() != payload.agent_identity()?
    {
        return Err(NativeError::Foreign);
    }
    Ok(new.instance())
}
