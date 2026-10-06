//! Bounded task metadata comparison and fakeable dispatch ordering, never executable approval.

use super::super::native_io::{NativeError, NativeResult};

pub const TASK_NAME: &str = "\\Crosspane\\Agent";
pub const SUPERVISOR_ARGUMENT: &str = "--windows-supervisor";
pub const MAX_XML_BYTES: usize = 64 * 1024;

/// The unchanged sealed constructor supplies all production context and image authority.
pub(super) fn native_start(trusted: &super::TrustedImages) -> NativeResult<()> {
    #[cfg(all(windows, not(test)))]
    {
        native::standalone(trusted)
    }
    #[cfg(any(not(windows), test))]
    {
        let _ = trusted;
        Err(NativeError::Unsupported)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunLevel {
    Limited,
    Highest,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Logon {
    InteractiveToken,
    Password,
    ServiceAccount,
}
#[derive(Clone, PartialEq, Eq)]
pub struct Definition {
    pub name: String,
    pub principal: String,
    pub trigger_user: String,
    pub logon: Logon,
    pub run_level: RunLevel,
    pub action: String,
    pub arguments: String,
    pub working_directory: String,
    pub logon_trigger_only: bool,
    pub ignore_new_instance: bool,
    pub manager_restart_count: u32,
    pub enabled: bool,
}
impl std::fmt::Debug for Definition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TaskDefinition")
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub definition: Definition,
    pub xml: String,
}
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TaskSnapshot")
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    Keep,
    PreserveDisabled,
    Register,
}

/// No native implementation may treat these caller-supplied observations as a capability.
pub fn plan(desired: &Definition, current: Option<&Snapshot>) -> NativeResult<Plan> {
    validate_desired(desired)?;
    let Some(current) = current else {
        return Ok(Plan::Register);
    };
    if current.xml.len() > MAX_XML_BYTES {
        return Err(NativeError::Oversize);
    }
    if current.xml.is_empty() || !same_role(desired, &current.definition) {
        return Err(NativeError::Foreign);
    }
    if !current.definition.enabled {
        return Ok(Plan::PreserveDisabled);
    }
    if current.definition.ignore_new_instance != desired.ignore_new_instance
        || current.definition.manager_restart_count != desired.manager_restart_count
    {
        return Ok(Plan::Register);
    }
    Ok(Plan::Keep)
}
fn validate_desired(desired: &Definition) -> NativeResult<()> {
    if desired.name != TASK_NAME
        || desired.principal.is_empty()
        || desired.principal.len() > 256
        || desired.principal != desired.trigger_user
        || desired.run_level != RunLevel::Limited
        || desired.logon != Logon::InteractiveToken
        || desired.arguments != SUPERVISOR_ARGUMENT
        || !desired.logon_trigger_only
        || !desired.ignore_new_instance
        || desired.manager_restart_count != 0
        || !desired.enabled
    {
        return Err(NativeError::Invalid);
    }
    let action = super::super::native_io::process::literal_path(&desired.action)?;
    let working = super::super::native_io::process::literal_path(&desired.working_directory)?;
    if action != format!("{working}\\crosspane-installer.exe") {
        return Err(NativeError::Invalid);
    }
    Ok(())
}
fn same_role(expected: &Definition, actual: &Definition) -> bool {
    // Native paths were already admitted from fixed roots. These comparisons grant no authority.
    expected.name == actual.name
        && expected.principal == actual.principal
        && expected.trigger_user == actual.trigger_user
        && expected.logon == actual.logon
        && expected.run_level == actual.run_level
        && expected.action == actual.action
        && expected.arguments == actual.arguments
        && expected.working_directory == actual.working_directory
        && expected.logon_trigger_only == actual.logon_trigger_only
}

pub trait TaskPort {
    fn inspect(&mut self) -> NativeResult<Option<Snapshot>>;
    fn record_intent(&mut self, original: Option<&Snapshot>) -> NativeResult<()>;
    fn register(&mut self, desired: &Definition) -> NativeResult<()>;
    fn record_result(&mut self) -> NativeResult<()>;
    fn retire(&mut self);
}

/// Shared production sequence; the port must enforce genuine admission before every native call.
pub fn reconcile(port: &mut impl TaskPort, desired: &Definition) -> NativeResult<Plan> {
    let original = port.inspect()?;
    let decision = plan(desired, original.as_ref())?;
    if decision != Plan::Register {
        return Ok(decision);
    }
    if let Err(error) = port.record_intent(original.as_ref()) {
        if error == NativeError::OutcomeUnknown {
            port.retire();
        }
        return Err(error);
    }
    if port.inspect()? != original {
        return Err(NativeError::Foreign);
    }
    let dispatched =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| port.register(desired)));
    match dispatched {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error != NativeError::OutcomeUnknown => return Err(error),
        _ => {
            port.retire();
            return Err(NativeError::OutcomeUnknown);
        }
    }
    if !matches!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| port.record_result())),
        Ok(Ok(()))
    ) {
        port.retire();
        return Err(NativeError::OutcomeUnknown);
    }
    Ok(decision)
}

#[cfg(all(windows, not(test)))]
mod native {
    use super::super::super::{
        native_io::{
            self, Cancellation, Deadline, InstallerLock, MonotonicClock, SupportProof,
            WindowsNativeIo,
            activation::{
                self, ActivationPort, Phase, SupervisorClaim, TaskActivationRecord, TaskSubmission,
            },
            process::{CallOwner, Dispatch},
        },
        payload::{health::VerifiedPayload, recovery},
    };
    use super::super::{NativeImages, TrustedImages};
    use super::*;
    use std::sync::{Arc, OnceLock};

    static OWNER: OnceLock<Arc<CallOwner>> = OnceLock::new();
    // At most one uncertain task dispatch in this process. Keep its genuine image/context pins;
    // a timed-out delivery never turns into permission to replace an unknown running image.
    static UNCERTAIN: OnceLock<Arc<NativeImages>> = OnceLock::new();
    pub(crate) struct UpgradeTaskSelection {
        io: Arc<WindowsNativeIo>,
        operation: [u8; 16],
        agent: native_io::files::FileIdentity,
        selected: Vec<u8>,
    }
    impl UpgradeTaskSelection {
        fn reverify(&self, deadline: &Deadline) -> NativeResult<()> {
            let proof = self.io.admit_support(deadline)?;
            let selected = recovery::selected_operation(&self.io, &proof, deadline)?
                .ok_or(NativeError::Foreign)?;
            if selected.operation() != self.operation
                || selected.phase() != recovery::Phase::StartIntent
                || serde_json::to_vec(&selected).map_err(|_| NativeError::Invalid)? != self.selected
            {
                return Err(NativeError::Foreign);
            }
            deadline.check()
        }
    }
    pub(in super::super) fn prepare_upgrade(
        io: Arc<WindowsNativeIo>,
        payload: &VerifiedPayload,
        deadline: &Deadline,
    ) -> NativeResult<UpgradeTaskSelection> {
        let proof = io.admit_support(deadline)?;
        payload.reverify(&io, &proof, deadline)?;
        let proof = io.admit_support(deadline)?;
        let selected =
            recovery::selected_operation(&io, &proof, deadline)?.ok_or(NativeError::Foreign)?;
        if selected.operation() != payload.operation()
            || selected.phase() != recovery::Phase::StartIntent
        {
            return Err(NativeError::Foreign);
        }
        let value = UpgradeTaskSelection {
            io,
            operation: payload.operation(),
            agent: payload.agent_identity()?,
            selected: serde_json::to_vec(&selected).map_err(|_| NativeError::Invalid)?,
        };
        value.reverify(deadline)?;
        Ok(value)
    }
    /// Task-submission operation correlation only; the scheduler GUID is durable in activation.
    /// Fresh owner/agent observation must supply health separately.
    pub(crate) struct TaskRunEvidence {
        operation: [u8; 16],
    }
    impl TaskRunEvidence {
        pub(crate) fn operation(&self) -> [u8; 16] {
            self.operation
        }
    }
    struct Binding {
        images: Arc<NativeImages>,
        upgrade: Option<UpgradeTaskSelection>,
        deadline: Deadline,
    }
    impl Binding {
        fn check(&self) -> NativeResult<()> {
            self.images.reverify(&self.deadline)?;
            if let Some(upgrade) = &self.upgrade {
                upgrade.reverify(&self.deadline)?;
                if self.images.agent.identity() != upgrade.agent {
                    return Err(NativeError::Foreign);
                }
            }
            self.deadline.check()
        }
        fn io(&self) -> &WindowsNativeIo {
            &self.images.io
        }
        fn user(&self) -> String {
            self.io().target().identity().user.sddl()
        }
        fn record(&self) -> NativeResult<Option<TaskActivationRecord>> {
            self.check()?;
            let proof = self.io().admit_support(&self.deadline)?;
            TaskActivationRecord::read(self.io(), &proof, &self.deadline)
        }
    }
    struct Port {
        binding: Binding,
        scheduler: native_io::task::Scheduler,
        lock: Option<InstallerLock>,
        operation: [u8; 16],
        original: Option<Snapshot>,
        record: Option<TaskActivationRecord>,
        retired: bool,
    }
    impl Port {
        fn publish(&self, record: &TaskActivationRecord) -> NativeResult<()> {
            self.binding.check()?;
            let proof = self.binding.io().admit_support(&self.binding.deadline)?;
            record.publish(
                self.binding.io(),
                &proof,
                self.lock.as_ref().ok_or(NativeError::Foreign)?,
                &self.binding.deadline,
            )
        }
        fn record_new(&self) -> NativeResult<TaskActivationRecord> {
            TaskActivationRecord::new(
                self.operation,
                self.binding.user(),
                self.binding.images.installer.identity(),
                self.original.as_ref().map(|r| r.xml.clone()),
            )
        }
    }
    impl TaskPort for Port {
        fn inspect(&mut self) -> NativeResult<Option<Snapshot>> {
            if self.retired {
                return Err(NativeError::OutcomeUnknown);
            }
            self.scheduler.inspect(&|| self.binding.check())
        }
        fn record_intent(&mut self, original: Option<&Snapshot>) -> NativeResult<()> {
            self.original = original.cloned();
            let record = self.record_new()?;
            self.publish(&record)?;
            self.record = Some(record);
            Ok(())
        }
        fn register(&mut self, desired: &Definition) -> NativeResult<()> {
            if self.retired {
                return Err(NativeError::OutcomeUnknown);
            }
            let binding = &self.binding;
            let lock = self.lock.as_ref().ok_or(NativeError::Foreign)?;
            let record = self.record.as_mut().ok_or(NativeError::Foreign)?;
            self.scheduler
                .register(
                    desired,
                    self.original.is_some(),
                    &|| binding.check(),
                    &mut |step| {
                        record.registration_step(step)?;
                        binding.check()?;
                        let proof = binding.io().admit_support(&binding.deadline)?;
                        record.publish(binding.io(), &proof, lock, &binding.deadline)
                    },
                )
                .map_err(|_| NativeError::OutcomeUnknown)
        }
        fn record_result(&mut self) -> NativeResult<()> {
            let mut record = self.record.clone().ok_or(NativeError::Invalid)?;
            record.registered()?;
            self.publish(&record)?;
            self.record = Some(record);
            Ok(())
        }
        fn retire(&mut self) {
            self.retired = true;
            OWNER
                .get_or_init(|| Arc::new(CallOwner::default()))
                .retire_mutations();
        }
    }
    impl ActivationPort for Port {
        fn prepare(&mut self) -> NativeResult<()> {
            self.binding.check()?;
            let proof = self.binding.io().admit_support(&self.binding.deadline)?;
            self.lock = Some(
                self.binding
                    .io()
                    .acquire_installer_lock(&proof, &self.binding.deadline)?,
            );
            if let Some(old) = self.binding.record()? {
                // A fresh selected upgrade StartIntent may supersede the previous activation;
                // standalone reopen never replays either registration or Run.
                if self.binding.upgrade.is_none() || old.operation() == self.operation {
                    return Err(NativeError::OutcomeUnknown);
                }
                if old.phase() != Phase::RunObserved || old.claim().is_none() {
                    return Err(NativeError::OutcomeUnknown);
                }
            }
            Ok(())
        }
        fn record_run_intent(&mut self) -> NativeResult<()> {
            if self.record.is_none() {
                self.original = self.inspect()?;
                let mut record = self.record_new()?;
                record.registered()?;
                self.record = Some(record);
            }
            let mut record = self.record.clone().ok_or(NativeError::Invalid)?;
            record.run_intent()?;
            self.publish(&record)?;
            self.record = Some(record);
            Ok(())
        }
        fn release_lock(&mut self) -> NativeResult<()> {
            self.binding.check()?;
            drop(self.lock.take().ok_or(NativeError::Foreign)?);
            Ok(())
        }
        fn run_once(&mut self) -> NativeResult<TaskSubmission> {
            if self.lock.is_some() || self.retired {
                return Err(NativeError::Foreign);
            }
            let current = self.binding.record()?.ok_or(NativeError::Foreign)?;
            current.bind(
                self.operation,
                &self.binding.user(),
                self.binding.images.installer.identity(),
            )?;
            if current.phase() != Phase::RunIntent || current.claim().is_some() {
                return Err(NativeError::Foreign);
            }
            let desired = desired(&self.binding);
            let actual = self.scheduler.inspect(&|| self.binding.check())?;
            if plan(&desired, actual.as_ref())? != Plan::Keep {
                return Err(NativeError::Foreign);
            }
            self.scheduler.run(&|| self.binding.check())
        }
        fn record_run_result(&mut self, submission: &TaskSubmission) -> NativeResult<()> {
            self.binding.check()?;
            let proof = self.binding.io().admit_support(&self.binding.deadline)?;
            self.lock = Some(
                self.binding
                    .io()
                    .acquire_installer_lock(&proof, &self.binding.deadline)?,
            );
            // The child may have claimed the RunIntent while Run was completing. Re-read and
            // preserve that exact claim instead of overwriting it with a stale parent snapshot.
            let mut record = self.binding.record()?.ok_or(NativeError::Foreign)?;
            record.bind(
                self.operation,
                &self.binding.user(),
                self.binding.images.installer.identity(),
            )?;
            record.run_observed(submission.guid().to_owned())?;
            self.publish(&record)?;
            Ok(())
        }
    }
    fn desired(binding: &Binding) -> Definition {
        Definition {
            name: TASK_NAME.into(),
            principal: binding.user(),
            trigger_user: binding.user(),
            logon: Logon::InteractiveToken,
            run_level: RunLevel::Limited,
            action: binding.images.installer.canonical_dos_path().into(),
            arguments: SUPERVISOR_ARGUMENT.into(),
            working_directory: binding.images._root.canonical_dos_path().into(),
            logon_trigger_only: true,
            ignore_new_instance: true,
            manager_restart_count: 0,
            enabled: true,
        }
    }
    fn start(
        images: Arc<NativeImages>,
        upgrade: Option<UpgradeTaskSelection>,
        deadline: &Deadline,
    ) -> NativeResult<TaskRunEvidence> {
        let operation = match &upgrade {
            Some(value) => value.operation,
            None => {
                let mut id = [0; 16];
                aws_lc_rs::rand::fill(&mut id).map_err(|_| NativeError::Unavailable)?;
                if id == [0; 16] {
                    return Err(NativeError::Unavailable);
                }
                id
            }
        };
        let retained = images.clone();
        let binding = Binding {
            images,
            upgrade,
            deadline: deadline.clone(),
        };
        binding.check()?;
        let budget = deadline.clone();
        let result = OWNER.get_or_init(|| Arc::new(CallOwner::default())).run(
            Dispatch::Mutation,
            deadline,
            move || {
                let scheduler = native_io::task::Scheduler::connect(&|| binding.check())?;
                let definition = desired(&binding);
                let mut port = Port {
                    binding,
                    scheduler,
                    lock: None,
                    operation,
                    original: None,
                    record: None,
                    retired: false,
                };
                let decision = activation::activate(&mut port, &definition)?;
                if decision == Plan::PreserveDisabled {
                    return Err(NativeError::Unsupported);
                }
                budget.check()?;
                Ok(TaskRunEvidence { operation })
            },
        );
        if matches!(result, Err(NativeError::OutcomeUnknown)) {
            let _ = UNCERTAIN.set(retained);
        }
        result
    }
    pub(super) fn standalone(trusted: &TrustedImages) -> NativeResult<()> {
        let clock: Arc<dyn native_io::Clock> = Arc::new(MonotonicClock::default());
        let deadline = Deadline::new(30_000, clock, Cancellation::default())?;
        start(trusted._native.clone(), None, &deadline).map(|_| ())
    }
    pub(in super::super) fn start_upgrade(
        trusted: &TrustedImages,
        selection: &UpgradeTaskSelection,
        deadline: &Deadline,
    ) -> NativeResult<TaskRunEvidence> {
        selection.reverify(deadline)?;
        let selection = UpgradeTaskSelection {
            io: selection.io.clone(),
            operation: selection.operation,
            agent: selection.agent,
            selected: selection.selected.clone(),
        };
        start(trusted._native.clone(), Some(selection), deadline)
    }

    /// Fresh own-process/image/context plus a once-published exact activation claim. Never decoded.
    pub(crate) struct TaskRunPermit {
        images: Arc<NativeImages>,
        operation: [u8; 16],
        user: String,
        owner: native_io::process::own::OwnProcessIdentity,
    }
    impl TaskRunPermit {
        pub(crate) fn operation(&self) -> [u8; 16] {
            self.operation
        }
        pub(crate) fn registration(&self) -> [u8; 16] {
            self.operation
        }
        pub(crate) fn user(&self) -> &str {
            &self.user
        }
        pub(crate) fn owner_identity(&self) -> &native_io::process::own::OwnProcessIdentity {
            &self.owner
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            // Arc clones retain this same original IO address; another valid context is foreign.
            if !std::ptr::eq(io, self.images.io.as_ref()) {
                return Err(NativeError::Foreign);
            }
            proof.check(io, deadline)?;
            self.images.reverify(deadline)?;
            self.owner.reverify(deadline)?;
            let proof = self.images.io.admit_support(deadline)?;
            let record = TaskActivationRecord::read(&self.images.io, &proof, deadline)?
                .ok_or(NativeError::Foreign)?;
            record.bind(self.operation, &self.user, self.images.installer.identity())?;
            if !matches!(record.phase(), Phase::RunIntent | Phase::RunObserved)
                || record.claim()
                    != Some(SupervisorClaim {
                        pid: self.owner.pid(),
                        creation: self.owner.creation(),
                    })
            {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
    }
    pub(in super::super) fn claim_supervisor(
        trusted: &TrustedImages,
        deadline: &Deadline,
    ) -> NativeResult<TaskRunPermit> {
        let images = trusted._native.clone();
        images.reverify(deadline)?;
        // Task entry must be THIS exact approved installed installer, not an outer installer
        // with identical bytes invoking the fixed argument from another pathname.
        if images.module.identity() != images.installer.identity()
            || images.module.canonical_dos_path() != images.installer.canonical_dos_path()
        {
            return Err(NativeError::Foreign);
        }
        let proof = images.io.admit_support(deadline)?;
        let owner = images.io.own_process_identity(&proof, deadline)?;
        let proof = images.io.admit_support(deadline)?;
        let lock = images.io.acquire_installer_lock(&proof, deadline)?;
        let proof = images.io.admit_support(deadline)?;
        let mut record = TaskActivationRecord::read(&images.io, &proof, deadline)?
            .ok_or(NativeError::Foreign)?;
        let user = images.io.target().identity().user.sddl();
        record.bind(record.operation(), &user, images.installer.identity())?;
        record.claim_supervisor(SupervisorClaim {
            pid: owner.pid(),
            creation: owner.creation(),
        })?;
        images.reverify(deadline)?;
        owner.reverify(deadline)?;
        let proof = images.io.admit_support(deadline)?;
        record.publish(&images.io, &proof, &lock, deadline)?;
        drop(lock);
        let permit = TaskRunPermit {
            operation: record.operation(),
            images,
            user,
            owner,
        };
        let proof = permit.images.io.admit_support(deadline)?;
        permit.reverify(&permit.images.io, &proof, deadline)?;
        Ok(permit)
    }
}
#[cfg(all(windows, not(test)))]
pub(crate) use native::TaskRunPermit;
#[cfg(all(windows, not(test)))]
pub(super) use native::{claim_supervisor, prepare_upgrade, start_upgrade};
