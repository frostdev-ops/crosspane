use super::*;
use crate::platform::linux::removal::{RemovalError, executor::OriginalRunning};
use crate::platform::linux::service::{LinuxService, ServiceFacts};
pub(super) const UNIT_INDEX: usize = 5;
pub(super) enum ManagerAction {
    Disable,
    Stop,
}
impl CleanupLease {
    /// Output is progress only; the original process watch supplies clean-exit evidence.
    pub fn stop(&self, service: Arc<LinuxService>, d: &Deadline) -> ManagerMutation {
        self.manager_observing(ManagerAction::Stop, d, move |d| {
            service.observe(d).map_err(|_| NativeError::Foreign)
        })
    }
    /// Checks the original after worker observation. systemctl still targets the unit: a restart
    /// between the final check and dispatch may be stopped, but never supplies clean authority.
    pub(crate) fn stop_original(
        &self,
        service: Arc<LinuxService>,
        original: OriginalRunning,
        d: &Deadline,
    ) -> ManagerMutation {
        self.manager_checked(
            ManagerAction::Stop,
            d,
            move |d| service.observe(d).map_err(|_| NativeError::Foreign),
            Some(original),
        )
    }
    /// One exact disable. Output is progress; effective state and clean exit are separate facts.
    pub fn disable(&self, service: Arc<LinuxService>, d: &Deadline) -> ManagerMutation {
        self.disable_observing(d, move |d| {
            service.observe(d).map_err(|_| NativeError::Foreign)
        })
    }
    pub(super) fn disable_observing(
        &self,
        d: &Deadline,
        observe: impl FnOnce(&Deadline) -> Result<ServiceFacts> + Send + 'static,
    ) -> ManagerMutation {
        self.manager_observing(ManagerAction::Disable, d, observe)
    }
    pub(super) fn manager_observing(
        &self,
        action: ManagerAction,
        d: &Deadline,
        observe: impl FnOnce(&Deadline) -> Result<ServiceFacts> + Send + 'static,
    ) -> ManagerMutation {
        self.manager_checked(action, d, observe, None)
    }
    fn manager_checked(
        &self,
        action: ManagerAction,
        d: &Deadline,
        observe: impl FnOnce(&Deadline) -> Result<ServiceFacts> + Send + 'static,
        original: Option<OriginalRunning>,
    ) -> ManagerMutation {
        self.dispatch(d, move |state, d| {
            let verb = match action {
                ManagerAction::Disable => "disable",
                ManagerAction::Stop => "stop",
            };
            let io = &state.proof.0.io;
            let environment = io.manager_environment(BTreeMap::new(), d)?;
            let before = observe(d)?;
            environment
                .manager
                .as_ref()
                .ok_or(NativeError::Foreign)?
                .revalidate(&io.target)?;
            let entries = state
                .proof
                .0
                .entries
                .lock()
                .map_err(|_| NativeError::Unavailable)?;
            let unit = &entries[UNIT_INDEX];
            if !unit.owned
                || unit.snapshot.hash() != Some(unit.hash)
                || before.fragment != unit.snapshot.path
                || before.source != io.target.source()
            {
                return Err(NativeError::Foreign);
            }
            if let Some(original) = original {
                if before.main_pid != original.pid() {
                    return Err(NativeError::Foreign);
                }
                original.revalidate(d).map_err(|e| match e {
                    RemovalError::Native(e) => e,
                    _ => NativeError::Foreign,
                })?;
            }
            CommandSpec::new(
                "/usr/bin/systemctl".into(),
                vec![
                    "--user".into(),
                    verb.into(),
                    "crosspane-agent.service".into(),
                ],
                environment,
                MAX_COMMAND_BYTES,
            )
        })
    }
    /// Only a genuine CleanAuthority command after explicit identity-deletion consent is permitted.
    /// Dispatch proves target/lifetime binding; it creates no clean-exit or deletion authority.
    pub fn erase_identity(&self, command: CommandSpec, d: &Deadline) -> ManagerMutation {
        self.dispatch(d, move |state, d| {
            let io = &state.proof.0.io;
            let entries = state
                .proof
                .0
                .entries
                .lock()
                .map_err(|_| NativeError::Unavailable)?;
            let agent = entries.first().ok_or(NativeError::Foreign)?;
            if !agent.owned
                || agent.snapshot.hash() != Some(agent.hash)
                || command.executable != io.target.agent_path()
                || command.argv != ["erase-identity"]
                || command.output_limit != 4096
                || !command
                    .agent
                    .as_ref()
                    .is_some_and(|a| a.cleanup_matches(&io.target, agent.hash))
            {
                return Err(NativeError::Foreign);
            }
            command
                .agent
                .as_ref()
                .ok_or(NativeError::Foreign)?
                .revalidate(&io.target, d)?;
            Ok(command)
        })
    }
    fn dispatch(
        &self,
        d: &Deadline,
        prepare: impl FnOnce(&mutation::State, &Deadline) -> Result<CommandSpec> + Send + 'static,
    ) -> ManagerMutation {
        let binding = match self.binding() {
            Ok(binding) => binding,
            Err(e) => {
                return ManagerMutation {
                    result: Err(e),
                    pending: None,
                };
            }
        };
        let pending = PendingOperation(binding.finished.clone());
        let worker_binding = binding.clone();
        let lease = self.clone();
        let deadline = d.clone();
        let result = bounded_launch(&PROCESS_LAUNCHES, d, move || {
            let result = (|| {
                worker_binding.check(&deadline)?;
                let mut command = prepare(&lease.0, &deadline)?;
                command.cleanup = Some(worker_binding.clone());
                lease.0.proof.0.io.execute(&command, &deadline, None)
            })();
            if matches!(result, Err(NativeError::OutcomeUnknown)) {
                lease.0.unknown.store(true, Ordering::Release);
            }
            result
        })
        .map_err(|e| {
            if matches!(e, NativeError::Timeout | NativeError::Cancelled) {
                NativeError::OutcomeUnknown
            } else {
                e
            }
        });
        #[cfg(test)]
        self.0.hook(&self.0.before_normalize);
        if matches!(result, Err(NativeError::OutcomeUnknown)) {
            self.0.unknown.store(true, Ordering::Release);
        }
        drop(binding);
        let pending = (!pending.completed()).then_some(pending);
        ManagerMutation { result, pending }
    }
}
