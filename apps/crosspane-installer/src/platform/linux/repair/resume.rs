//! Bounded repair intent. Stored identities disclose expected resources; SupportProof remains
//! the sole creation authority. A recovered journal cannot reconstruct the original process watch.
use super::super::native_io::TargetPaths;
use super::super::payload::MAX_RECORD_BYTES;
use super::*;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    version: u8,
    operation: u64,
    revision: u64,
    manifest: [u8; 32],
    original_instance: u64,
    stage: RepairStage,
    target: StoredTarget,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredTarget {
    uid: u32,
    roots: [std::path::PathBuf; 6],
    runtime_override: Option<std::path::PathBuf>,
    scratch: bool,
}
impl StoredTarget {
    fn of(io: &LinuxNativeIo) -> Self {
        let TargetPaths {
            uid,
            home,
            prefix,
            config_home,
            state_home,
            data_home,
            runtime_home,
            runtime_override,
        } = io.target().paths();
        Self {
            uid: *uid,
            roots: [
                home.clone(),
                prefix.clone(),
                config_home.clone(),
                state_home.clone(),
                data_home.clone(),
                runtime_home.clone(),
            ],
            runtime_override: runtime_override.clone(),
            scratch: io.target().source() == crate::agent_contract::ObservationSource::Demo,
        }
    }
}
impl Journal {
    pub(super) fn new(io: &LinuxNativeIo, plan: &RepairPlan) -> Result<Self> {
        Ok(Self {
            version: 1,
            operation: plan.operation().0,
            revision: plan.revision(),
            manifest: plan.facts().package_manifest_sha256,
            original_instance: plan.facts().instance_id.ok_or(RemovalError::NotClean)?,
            stage: RepairStage::Recorded,
            target: StoredTarget::of(io),
        })
    }
    pub(super) fn operation(&self) -> OperationId {
        OperationId(self.operation)
    }
    pub(super) fn original_instance(&self) -> u64 {
        self.original_instance
    }
    pub(super) fn stage(&self) -> RepairStage {
        self.stage
    }
    pub(super) fn advance(&mut self, stage: RepairStage) {
        self.stage = stage;
    }
    fn path(io: &LinuxNativeIo) -> std::path::PathBuf {
        io.target()
            .paths()
            .state_home
            .join("crosspane/installer/repair-intent.json")
    }
    fn bound(&self, io: &LinuxNativeIo) -> Result<()> {
        if self.version != 1
            || self.operation == 0
            || self.revision == 0
            || self.original_instance == 0
            || self.target != StoredTarget::of(io)
        {
            return Err(NativeError::Foreign.into());
        }
        Ok(())
    }
    pub(super) fn check_package(&self, package: &Package) -> Result<()> {
        let manifest = super::super::payload::sha256(
            &serde_json::to_vec(package.manifest()).map_err(|_| NativeError::Invalid)?,
        );
        if self.manifest != manifest {
            return Err(NativeError::Foreign.into());
        }
        Ok(())
    }
    pub(super) fn validate_cleanup(
        proof: &super::super::native_io::CleanupProof,
        bytes: &[u8],
    ) -> Result<()> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(NativeError::Oversize.into());
        }
        let journal: Self = serde_json::from_slice(bytes).map_err(|_| NativeError::Invalid)?;
        let t = journal.target;
        let [
            home,
            prefix,
            config_home,
            state_home,
            data_home,
            runtime_home,
        ] = t.roots;
        let paths = TargetPaths {
            uid: t.uid,
            home,
            prefix,
            config_home,
            state_home,
            data_home,
            runtime_home,
            runtime_override: t.runtime_override,
        };
        if journal.version != 1
            || journal.operation == 0
            || journal.revision == 0
            || journal.original_instance == 0
            || !proof.repair_target_matches(&paths, t.scratch)
        {
            return Err(NativeError::Foreign.into());
        }
        Ok(())
    }
    pub(super) fn decode(io: &LinuxNativeIo, bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(NativeError::Oversize.into());
        }
        let journal: Self = serde_json::from_slice(bytes).map_err(|_| NativeError::Invalid)?;
        journal.bound(io)?;
        Ok(journal)
    }
    pub(super) fn load(io: &LinuxNativeIo, input: &RepairInput<'_>) -> Result<Option<Self>> {
        input.deadline.check()?;
        input.proof.check(io)?;
        let path = Self::path(io);
        if io.metadata(&path)?.is_none() {
            return Ok(None);
        }
        let bytes = io.read(&path, MAX_RECORD_BYTES, true)?;
        let journal = Self::decode(io, &bytes)?;
        input.deadline.check()?;
        input.proof.check(io)?;
        Ok(Some(journal))
    }
    /// The caller does not retain this lock across payload or service helper dispatch.
    pub(super) fn create(&self, io: &LinuxNativeIo, input: &RepairInput<'_>) -> Result<()> {
        let _lease = io.install_lease(input.proof)?;
        if let Some(previous) = Self::load(io, input)?
            && (previous.stage != RepairStage::Verified
                || self.operation <= previous.operation
                || self.revision <= previous.revision)
        {
            return Err(RepairError::RecoveryPending);
        }
        self.write(io, input)
    }
    pub(super) fn checkpoint(
        &self,
        io: &LinuxNativeIo,
        input: &RepairInput<'_>,
        previous: RepairStage,
    ) -> Result<()> {
        let _lease = io.install_lease(input.proof)?;
        let current = Self::load(io, input)?.ok_or(RepairError::RecoveryPending)?;
        let mut expected = self.clone();
        expected.stage = previous;
        if current != expected {
            return Err(RepairError::RecoveryPending);
        }
        self.write(io, input)
    }
    fn write(&self, io: &LinuxNativeIo, input: &RepairInput<'_>) -> Result<()> {
        self.bound(io)?;
        self.check_package(input.package)?;
        input.deadline.check()?;
        let bytes = serde_json::to_vec(self).map_err(|_| NativeError::Invalid)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(NativeError::Oversize.into());
        }
        io.atomic_write(input.proof, &Self::path(io), &bytes)?;
        input.deadline.check()?;
        Ok(())
    }
}
