use super::*;
use crate::platform::linux::service::{LinuxService, ServiceFacts};
pub(super) const UNIT_INDEX: usize = 5;
impl CleanupLease {
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
                let state = &lease.0;
                let d = &deadline;
                worker_binding.check(d)?;
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
                    || before.source != state.proof.0.io.target.source()
                {
                    return Err(NativeError::Foreign);
                }
                drop(entries);
                let mut command = CommandSpec::new(
                    "/usr/bin/systemctl".into(),
                    vec![
                        "--user".into(),
                        "disable".into(),
                        "crosspane-agent.service".into(),
                    ],
                    environment,
                    MAX_COMMAND_BYTES,
                )?;
                command.cleanup = Some(worker_binding.clone());
                io.execute(&command, d, None)
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
