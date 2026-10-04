use super::*;
use crate::agent_contract::{
    BootstrapPhase, BootstrapV1, EraseIdentityV1, LastExitV1, parse_bootstrap,
    parse_erase_identity, parse_last_exit,
};
use std::sync::Arc;

pub struct TrackedAgent {
    io: Arc<LinuxNativeIo>,
    pub(super) bootstrap: BootstrapV1,
    watch: ProcessWatch,
}
impl std::fmt::Debug for TrackedAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TrackedAgent { .. }")
    }
}
impl TrackedAgent {
    /// A running-only original binding for a retained worker; grants no clean/erase capability.
    pub(crate) fn running_check(&self) -> OriginalRunning {
        OriginalRunning {
            io: self.io.clone(),
            bootstrap: self.bootstrap.clone(),
            watch: self.watch.clone(),
        }
    }
    pub(crate) fn target_binding(&self) -> super::super::native_io::TargetBinding {
        self.io.target_binding()
    }
    /// The original process and bootstrap must still agree immediately before stop dispatch.
    pub(crate) fn revalidate_running(&self, deadline: &Deadline) -> Result<()> {
        Ok(self
            .io
            .revalidate_original_running(&self.watch, &self.bootstrap, deadline)?)
    }
    pub fn capture(io: Arc<LinuxNativeIo>, deadline: &Deadline) -> Result<Self> {
        Self::capture_with(io, None, deadline)
    }
    pub fn scratch_capture(
        io: Arc<LinuxNativeIo>,
        reader: Arc<dyn ExitReader>,
        deadline: &Deadline,
    ) -> Result<Self> {
        Self::capture_with(io, Some(reader), deadline)
    }
    fn capture_with(
        io: Arc<LinuxNativeIo>,
        reader: Option<Arc<dyn ExitReader>>,
        deadline: &Deadline,
    ) -> Result<Self> {
        let (bootstrap, watch) = io.track_bootstrap(reader, deadline)?;
        Ok(Self {
            io,
            bootstrap,
            watch,
        })
    }
    pub fn instance_id(&self) -> u64 {
        self.bootstrap.instance_id
    }
    pub fn original(&self) -> &ProcessIdentity {
        self.watch.original()
    }
    /// No socket, manager result, receipt-only reconstruction or unknown process grants this token.
    pub fn clean_authority(self: &Arc<Self>, deadline: &Deadline) -> Result<CleanAuthority> {
        let receipt = self.check(deadline)?;
        Ok(CleanAuthority {
            original: self.clone(),
            receipt,
        })
    }
    fn check(&self, deadline: &Deadline) -> Result<LastExitV1> {
        Ok(self.clean_snapshot(deadline)?.1)
    }
    fn clean_snapshot(&self, deadline: &Deadline) -> Result<(BootstrapV1, LastExitV1)> {
        deadline.check()?;
        let (bootstrap_bytes, exit_bytes, observed) =
            self.watch.exit_observation(&self.io, deadline)?;
        let bootstrap = parse_bootstrap(&bootstrap_bytes)?;
        let receipt = parse_last_exit(&exit_bytes)?;
        if bootstrap.instance_id != self.bootstrap.instance_id
            || bootstrap.pid != self.bootstrap.pid
            || bootstrap.started_unix_ms != self.bootstrap.started_unix_ms
            || bootstrap.runtime_dir != self.bootstrap.runtime_dir
            || bootstrap.phase_seq < self.bootstrap.phase_seq
            || bootstrap.phase != BootstrapPhase::Ready
            || !receipt.clean
            || receipt.instance_id != self.bootstrap.instance_id
            || receipt.stopped_unix_ms < self.bootstrap.started_unix_ms
            || observed != ProcessExit::Exited
        {
            return Err(RemovalError::NotClean);
        }
        deadline.check()?;
        Ok((bootstrap, receipt))
    }
}
/// Non-cloneable, running-only worker capability; never serialized or caller-constructed.
pub(crate) struct OriginalRunning {
    io: Arc<LinuxNativeIo>,
    bootstrap: BootstrapV1,
    watch: ProcessWatch,
}
impl std::fmt::Debug for OriginalRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OriginalRunning(..)")
    }
}
impl OriginalRunning {
    pub(crate) fn pid(&self) -> u32 {
        self.watch.original().pid
    }
    pub(crate) fn revalidate(&self, deadline: &Deadline) -> Result<()> {
        Ok(self
            .io
            .revalidate_original_running(&self.watch, &self.bootstrap, deadline)?)
    }
}
/// Ephemeral original-process and literal-receipt authority; never serialized or caller-constructed.
pub struct CleanAuthority {
    original: Arc<TrackedAgent>,
    receipt: LastExitV1,
}
impl std::fmt::Debug for CleanAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CleanAuthority { .. }")
    }
}
impl CleanAuthority {
    /// The original watch and literal clean receipt must belong to this exact I/O context.
    pub fn clean_stop(&self, io: &LinuxNativeIo, deadline: &Deadline) -> Result<CleanStop> {
        if self.original.target_binding() != io.target_binding() {
            return Err(NativeError::Foreign.into());
        }
        io.validate_target()?;
        let (bootstrap, receipt) = self.original.clean_snapshot(deadline)?;
        if receipt != self.receipt {
            return Err(RemovalError::Stale);
        }
        Ok(CleanStop {
            binding: io.target_binding(),
            original: self.original.clone(),
            bootstrap,
            receipt,
        })
    }
    pub fn receipt(&self) -> &LastExitV1 {
        &self.receipt
    }
    pub fn revalidate(&self, deadline: &Deadline) -> Result<()> {
        if self.original.check(deadline)? != self.receipt {
            return Err(RemovalError::Stale);
        }
        Ok(())
    }
    /// Recheck immediately before dispatch too. The producer's agent.lock still decides races
    /// with a new process; this token promises no cross-process atomic exclusion.
    pub fn erase_command(
        &self,
        digest: [u8; 32],
        environment: ChildEnvironment,
        deadline: &Deadline,
    ) -> Result<CommandSpec> {
        self.revalidate(deadline)?;
        Ok(CommandSpec::erase_identity(
            &self.original.io,
            digest,
            environment,
            deadline,
        )?)
    }
}
/// One repair's ephemeral clean exit, retaining the original watch/process and exact bootstrap.
/// Neither paths nor a receipt reconstructed after exit can create this proof.
pub struct CleanStop {
    binding: super::super::native_io::TargetBinding,
    original: Arc<TrackedAgent>,
    bootstrap: BootstrapV1,
    receipt: LastExitV1,
}
impl std::fmt::Debug for CleanStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CleanStop { .. }")
    }
}
impl CleanStop {
    pub fn instance_id(&self) -> u64 {
        self.bootstrap.instance_id
    }
    /// Checks context first, then the original exit and byte-equivalent parsed bootstrap/receipt.
    /// A new bootstrap invalidates reuse. This makes no old-process liveness admission.
    pub(crate) fn revalidate_for(
        &self,
        io: &LinuxNativeIo,
        deadline: &Deadline,
    ) -> std::result::Result<(), NativeError> {
        if self.binding != io.target_binding() {
            return Err(NativeError::Foreign);
        }
        io.validate_target()?;
        let (bootstrap, receipt) =
            self.original
                .clean_snapshot(deadline)
                .map_err(|error| match error {
                    RemovalError::Native(error) => error,
                    _ => NativeError::Foreign,
                })?;
        if bootstrap != self.bootstrap || receipt != self.receipt {
            return Err(NativeError::Foreign);
        }
        deadline.check()
    }
}
/// Stdout is semantic truth only: refusal/waiting/failed and kept trust remain literal outcomes.
pub fn admit_erase_output(output: &CommandOutput) -> Result<EraseIdentityV1> {
    if output.code != Some(0) || output.stdout.len() + output.stderr.len() > 4096 {
        return Err(RemovalError::Native(NativeError::Unavailable));
    }
    Ok(parse_erase_identity(&output.stdout)?)
}
