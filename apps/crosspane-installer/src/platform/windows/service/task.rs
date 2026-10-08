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
    static UNCERTAIN_FIRST: OnceLock<Arc<FirstInstallTaskSelection>> = OnceLock::new();
    /// Actual verified first payload plus retained original absence reservation. No facts factory.
    #[derive(Clone)]
    pub(crate) struct FirstInstallTaskSelection {
        io: Arc<WindowsNativeIo>,
        payload: Arc<VerifiedPayload>,
        reservation: native_io::FirstInstallReservation,
        selected: super::super::super::first_install::record::FirstInstallRecord,
        released: Arc<std::sync::atomic::AtomicBool>,
    }
    impl FirstInstallTaskSelection {
        fn reverify(&self, deadline: &Deadline) -> NativeResult<()> {
            use super::super::super::first_install::record::Phase as FirstPhase;
            let proof = self.io.admit_support(deadline)?;
            self.payload.reverify(&self.io, &proof, deadline)?;
            if !self.released.load(std::sync::atomic::Ordering::Acquire) {
                self.reservation.reverify(&self.io, &proof, deadline)?;
            }
            let actual = self
                .io
                .read_first_install(&proof, deadline)?
                .ok_or(NativeError::Missing)?;
            if !actual.same_selection(&self.selected)
                || actual.phase().rank() < FirstPhase::TaskIntent.rank()
                || actual.phase().rank() > FirstPhase::RunObserved.rank()
            {
                return Err(NativeError::Foreign);
            }
            for role in super::super::super::payload::inventory::PayloadRole::ALL {
                if actual.role(role)? != self.selected.role(role)?
                    || actual.role(role)?.approved != *self.payload.pin(role)?.facts()
                {
                    return Err(NativeError::Foreign);
                }
            }
            deadline.check()
        }
        fn checkpoint(
            &self,
            lock: &InstallerLock,
            phase: super::super::super::first_install::record::Phase,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.reverify(deadline)?;
            let proof = self.io.admit_support(deadline)?;
            self.io.verify_first_lock(&proof, lock, deadline)?;
            let mut actual = self
                .io
                .read_first_install(&proof, deadline)?
                .ok_or(NativeError::Missing)?;
            actual.advance(phase)?;
            if self.released.load(std::sync::atomic::Ordering::Acquire) {
                // Only this actual prepared/consumed activation can publish the Run result.
                let publication = self.io.publish_record(
                    &proof,
                    lock,
                    native_io::records::RecordName::FirstInstall,
                    &actual.encode()?,
                    deadline,
                )?;
                if publication.native_failure.is_some()
                    || publication.state != native_io::records::PublicationRecovery::NewPublished
                {
                    return Err(NativeError::OutcomeUnknown);
                }
            } else {
                self.io.publish_first_install(
                    &proof,
                    lock,
                    &self.reservation,
                    &actual,
                    deadline,
                )?;
            }
            Ok(())
        }
        fn release(&self, deadline: &Deadline) -> NativeResult<()> {
            self.reverify(deadline)?;
            let proof = self.io.admit_support(deadline)?;
            self.reservation.release(&proof, deadline)?;
            self.released
                .store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        }
    }
    pub(in super::super) fn prepare_first_install(
        io: Arc<WindowsNativeIo>,
        payload: Arc<VerifiedPayload>,
        reservation: native_io::FirstInstallReservation,
        deadline: &Deadline,
    ) -> NativeResult<FirstInstallTaskSelection> {
        use super::super::super::first_install::record::Phase as FirstPhase;
        let proof = io.admit_support(deadline)?;
        payload.reverify(&io, &proof, deadline)?;
        reservation.reverify(&io, &proof, deadline)?;
        let selected = io
            .read_first_install(&proof, deadline)?
            .ok_or(NativeError::Missing)?;
        if selected.phase() != FirstPhase::TaskIntent || selected.operation() != payload.operation()
        {
            return Err(NativeError::Foreign);
        }
        let value = FirstInstallTaskSelection {
            io,
            payload,
            reservation,
            selected,
            released: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        value.reverify(deadline)?;
        Ok(value)
    }
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
    /// A retained genuine fixed-payload/completion gate on the ORIGINAL source IO. Captured
    /// StartIntent bytes correlate the selection; they never replace native approval or tree proof.
    #[derive(Clone)]
    pub(crate) struct RepairTaskSelection {
        io: Arc<WindowsNativeIo>,
        payload: Arc<native_io::RepairFixedPayload>,
        selected: Vec<u8>,
        operation: [u8; 16],
    }
    impl RepairTaskSelection {
        fn reverify(&self, deadline: &Deadline) -> NativeResult<()> {
            let proof = self.io.admit_support(deadline)?;
            self.payload.reverify(&self.io, &proof, deadline)?;
            self.payload
                .reverify_completion(&self.io, &proof, deadline)?;
            let record = self
                .io
                .read_payload_repair(&proof, deadline)?
                .ok_or(NativeError::Foreign)?;
            if record.operation() != self.operation
                || record.phase()
                    != super::super::super::repair::payload_record::PayloadRepairPhase::StartIntent
                || record.encode()? != self.selected
            {
                return Err(NativeError::Foreign);
            }
            deadline.check()
        }
        fn matches_task(&self, actual: Option<&Snapshot>) -> NativeResult<()> {
            let selected =
                super::super::super::repair::payload_record::PayloadRepairRecord::decode(
                    &self.selected,
                )?;
            match actual {
                Some(actual)
                    if actual.definition.enabled && actual.xml == selected.task().xml() =>
                {
                    Ok(())
                }
                None if selected.task().diagnostic()
                    == super::super::super::repair::RepairDiagnostic::Missing =>
                {
                    Ok(())
                }
                _ => Err(NativeError::Foreign),
            }
        }
        fn matches_images(&self, images: &NativeImages, deadline: &Deadline) -> NativeResult<()> {
            use super::super::super::payload::inventory::PayloadRole;
            self.reverify(deadline)?;
            let proof = images.io.admit_support(deadline)?;
            for (role, pin) in [
                (PayloadRole::Installer, &images.installer),
                (PayloadRole::Agent, &images.agent),
                (PayloadRole::Ui, &images.ui),
            ] {
                pin.reverify(&images.io, &proof, deadline)?;
                if pin.identity() != self.payload.identity(role)
                    || pin.approved().facts() != self.payload.facts(role)
                {
                    return Err(NativeError::Foreign);
                }
            }
            let inventory = super::super::super::payload::inventory::ApprovedInventory::embedded()?;
            let ctl = images._root.open_approved(
                &images.io,
                &proof,
                PayloadRole::Ctl,
                inventory.role(PayloadRole::Ctl)?,
                deadline,
            )?;
            if ctl.identity() != self.payload.identity(PayloadRole::Ctl)
                || ctl.approved().facts() != self.payload.facts(PayloadRole::Ctl)
            {
                return Err(NativeError::Foreign);
            }
            ctl.reverify(&images.io, &proof, deadline)
        }
    }
    pub(in super::super) fn prepare_repair(
        io: Arc<WindowsNativeIo>,
        payload: &native_io::RepairFixedPayload,
        deadline: &Deadline,
    ) -> NativeResult<RepairTaskSelection> {
        if !Arc::ptr_eq(&io, payload.io()) {
            return Err(NativeError::Foreign);
        }
        let proof = io.admit_support(deadline)?;
        payload.reverify(&io, &proof, deadline)?;
        payload.reverify_completion(&io, &proof, deadline)?;
        let record = io
            .read_payload_repair(&proof, deadline)?
            .ok_or(NativeError::Foreign)?;
        if record.operation() != payload.operation()
            || record.phase()
                != super::super::super::repair::payload_record::PayloadRepairPhase::StartIntent
        {
            return Err(NativeError::Foreign);
        }
        let selection = RepairTaskSelection {
            operation: payload.operation(),
            selected: record.encode()?,
            payload: payload.retain_for_start(),
            io,
        };
        selection.reverify(deadline)?;
        Ok(selection)
    }
    // One uncertain repair task call keeps the genuine source completion and fixed-file pins.
    static UNCERTAIN_REPAIR: OnceLock<Arc<RepairTaskSelection>> = OnceLock::new();

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
        repair: Option<RepairTaskSelection>,
        first: Option<FirstInstallTaskSelection>,
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
            if let Some(repair) = &self.repair {
                repair.matches_images(&self.images, &self.deadline)?;
            }
            if let Some(first) = &self.first {
                first.reverify(&self.deadline)?;
                for (role, image) in [
                    (
                        super::super::super::payload::inventory::PayloadRole::Installer,
                        &self.images.installer,
                    ),
                    (
                        super::super::super::payload::inventory::PayloadRole::Agent,
                        &self.images.agent,
                    ),
                    (
                        super::super::super::payload::inventory::PayloadRole::Ui,
                        &self.images.ui,
                    ),
                ] {
                    if Some(image.identity().into())
                        != first
                            .selected
                            .role(role)?
                            .published
                            .as_ref()
                            .map(|r| r.identity)
                    {
                        return Err(NativeError::Foreign);
                    }
                }
            }
            self.deadline.check()
        }
        fn io(&self) -> &WindowsNativeIo {
            match &self.first {
                Some(first) => &first.io,
                None => &self.images.io,
            }
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
            let actual = self.scheduler.inspect(&|| self.binding.check())?;
            if self.binding.first.is_some() && self.record.is_none() && actual.is_some() {
                return Err(NativeError::Foreign);
            }
            if let Some(repair) = &self.binding.repair {
                repair.matches_task(actual.as_ref())?;
            }
            Ok(actual)
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
            if let Some(first) = &self.binding.first {
                first.checkpoint(
                    self.lock.as_ref().ok_or(NativeError::Foreign)?,
                    super::super::super::first_install::record::Phase::TaskRegistered,
                    &self.binding.deadline,
                )?;
            }
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
                if (self.binding.upgrade.is_none() && self.binding.repair.is_none())
                    || old.operation() == self.operation
                {
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
            if let Some(first) = &self.binding.first {
                first.checkpoint(
                    self.lock.as_ref().ok_or(NativeError::Foreign)?,
                    super::super::super::first_install::record::Phase::RunIntent,
                    &self.binding.deadline,
                )?;
            }
            Ok(())
        }
        fn release_lock(&mut self) -> NativeResult<()> {
            self.binding.check()?;
            if let Some(first) = &self.binding.first {
                first.release(&self.binding.deadline)?;
            }
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
            if let Some(repair) = &self.binding.repair {
                repair.matches_task(actual.as_ref())?;
            }
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
            if let Some(first) = &self.binding.first {
                first.checkpoint(
                    self.lock.as_ref().ok_or(NativeError::Foreign)?,
                    super::super::super::first_install::record::Phase::RunObserved,
                    &self.binding.deadline,
                )?;
            }
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
        repair: Option<RepairTaskSelection>,
        deadline: &Deadline,
    ) -> NativeResult<TaskRunEvidence> {
        start_selected(images, upgrade, repair, None, deadline)
    }
    fn start_selected(
        images: Arc<NativeImages>,
        upgrade: Option<UpgradeTaskSelection>,
        repair: Option<RepairTaskSelection>,
        first: Option<FirstInstallTaskSelection>,
        deadline: &Deadline,
    ) -> NativeResult<TaskRunEvidence> {
        if first.is_some() && (upgrade.is_some() || repair.is_some()) {
            return Err(NativeError::Foreign);
        }
        let operation = match (&upgrade, &repair) {
            (Some(_), Some(_)) => return Err(NativeError::Foreign),
            (None, Some(value)) => value.operation,
            (Some(value), None) => value.operation,
            (None, None) => {
                if let Some(first) = &first {
                    first.selected.operation()
                } else {
                    let mut id = [0; 16];
                    aws_lc_rs::rand::fill(&mut id).map_err(|_| NativeError::Unavailable)?;
                    if id == [0; 16] {
                        return Err(NativeError::Unavailable);
                    }
                    id
                }
            }
        };
        let retained = images.clone();
        let retained_repair = repair.clone();
        let retained_first = first.clone();
        let binding = Binding {
            images,
            upgrade,
            repair,
            first,
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
            if let Some(selection) = retained_repair {
                let _ = UNCERTAIN_REPAIR.set(Arc::new(selection));
            }
            if let Some(selection) = retained_first {
                let _ = UNCERTAIN_FIRST.set(Arc::new(selection));
            }
        }
        result
    }
    pub(super) fn standalone(trusted: &TrustedImages) -> NativeResult<()> {
        let clock: Arc<dyn native_io::Clock> = Arc::new(MonotonicClock::default());
        let deadline = Deadline::new(30_000, clock, Cancellation::default())?;
        start(trusted._native.clone(), None, None, &deadline).map(|_| ())
    }
    pub(in super::super) fn start_first_install(
        trusted: &TrustedImages,
        selection: FirstInstallTaskSelection,
        deadline: &Deadline,
    ) -> NativeResult<TaskRunEvidence> {
        selection.reverify(deadline)?;
        start_selected(
            trusted._native.clone(),
            None,
            None,
            Some(selection),
            deadline,
        )
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
        start(trusted._native.clone(), Some(selection), None, deadline)
    }

    pub(in super::super) fn start_repair(
        trusted: &TrustedImages,
        selection: &RepairTaskSelection,
        deadline: &Deadline,
    ) -> NativeResult<TaskRunEvidence> {
        selection.matches_images(&trusted._native, deadline)?;
        start(
            trusted._native.clone(),
            None,
            Some(selection.clone()),
            deadline,
        )
    }
    /// The new supervisor's independent four-image admission. This is not the source process's
    /// old tree proof; the genuine task claim and exclusive namespace remain separately required.
    struct RepairClaim {
        selected: super::super::super::repair::payload_record::PayloadRepairRecord,
        ctl: native_io::OpenedPe,
    }
    impl RepairClaim {
        fn admit(
            images: &NativeImages,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<Option<Self>> {
            use super::super::super::{
                payload::inventory::{ApprovedInventory, PayloadRole},
                repair::payload_record::PayloadRepairPhase,
            };
            let proof = images.io.admit_support(deadline)?;
            let Some(selected) = images.io.read_payload_repair(&proof, deadline)? else {
                return Ok(None);
            };
            if selected.operation() != operation {
                return Ok(None);
            }
            if !matches!(
                selected.phase(),
                PayloadRepairPhase::StartIntent | PayloadRepairPhase::StartSubmitted
            ) {
                return Err(NativeError::Foreign);
            }
            selected.context().matches(images.io.target().identity())?;
            let inventory = ApprovedInventory::embedded()?;
            let ctl = images._root.open_approved(
                &images.io,
                &proof,
                PayloadRole::Ctl,
                inventory.role(PayloadRole::Ctl)?,
                deadline,
            )?;
            let value = Self { selected, ctl };
            value.reverify(images, deadline)?;
            Ok(Some(value))
        }
        fn reverify(&self, images: &NativeImages, deadline: &Deadline) -> NativeResult<()> {
            use super::super::super::{
                payload::inventory::PayloadRole, repair::payload_record::PayloadRepairPhase,
            };
            let proof = images.io.admit_support(deadline)?;
            let actual = images
                .io
                .read_payload_repair(&proof, deadline)?
                .ok_or(NativeError::Foreign)?;
            if !actual.same_selection(&self.selected)
                || matches!(
                    actual.phase(),
                    PayloadRepairPhase::Unknown | PayloadRepairPhase::Cancelled
                )
                || actual.phase().rank() < PayloadRepairPhase::StartIntent.rank()
            {
                return Err(NativeError::Foreign);
            }
            actual.context().matches(images.io.target().identity())?;
            for (role, pin) in [
                (PayloadRole::Installer, &images.installer),
                (PayloadRole::Agent, &images.agent),
                (PayloadRole::Ui, &images.ui),
                (PayloadRole::Ctl, &self.ctl),
            ] {
                pin.reverify(&images.io, &proof, deadline)?;
                let selected = actual.role(role).published().ok_or(NativeError::Foreign)?;
                if pin.identity()
                    != (native_io::files::FileIdentity {
                        volume: selected.identity.volume,
                        file: selected.identity.file,
                    })
                    || pin.approved().facts() != &selected.facts
                    || pin.approved().facts() != actual.selection().source(role)
                {
                    return Err(NativeError::Foreign);
                }
            }
            deadline.check()
        }
    }
    /// Fresh own-process/image/context plus a once-published exact activation claim. Never decoded.
    pub(crate) struct TaskRunPermit {
        images: Arc<NativeImages>,
        operation: [u8; 16],
        user: String,
        owner: native_io::process::own::OwnProcessIdentity,
        repair: Option<RepairClaim>,
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
        pub(crate) fn is_repair(&self) -> bool {
            self.repair.is_some()
        }
        pub(crate) fn repair_selection(
            &self,
        ) -> NativeResult<&super::super::super::repair::payload_record::PayloadRepairRecord>
        {
            self.repair
                .as_ref()
                .map(|r| &r.selected)
                .ok_or(NativeError::Foreign)
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
            if let Some(repair) = &self.repair {
                repair.reverify(&self.images, deadline)?;
            }
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
        let repair = RepairClaim::admit(&images, record.operation(), deadline)?;
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
            repair,
        };
        let proof = permit.images.io.admit_support(deadline)?;
        permit.reverify(&permit.images.io, &proof, deadline)?;
        Ok(permit)
    }
}
#[cfg(all(windows, not(test)))]
pub(crate) use native::TaskRunPermit;
#[cfg(all(windows, not(test)))]
pub(super) use native::{
    TaskRunEvidence, claim_supervisor, prepare_first_install, prepare_repair, prepare_upgrade,
    start_first_install, start_repair, start_upgrade,
};

// A6 repair is separate from activation: registration-only, no TaskActivation claim or Run.
#[cfg(all(windows, not(test)))]
mod repair_native {
    use super::super::super::{
        native_io::{
            Deadline, RepairTaskBinding, WindowsNativeIo,
            process::{CallOwner, Dispatch},
            task::{RepairTaskFailure, Scheduler},
        },
        repair::{RepairDiagnostic, RepairTaskObservation},
    };
    use super::*;
    use std::sync::{Arc, OnceLock};
    static PROBE: OnceLock<Arc<CallOwner>> = OnceLock::new();
    static REGISTER: OnceLock<Arc<CallOwner>> = OnceLock::new();
    static UNCERTAIN: OnceLock<Arc<RepairTaskBinding>> = OnceLock::new();
    fn diagnostic(error: NativeError) -> RepairDiagnostic {
        match error {
            NativeError::Foreign => RepairDiagnostic::UnsafeForeign,
            NativeError::Missing => RepairDiagnostic::Missing,
            NativeError::Unavailable | NativeError::Busy => RepairDiagnostic::Unavailable,
            _ => RepairDiagnostic::Unknown,
        }
    }
    fn failed(error: RepairTaskFailure) -> NativeResult<RepairTaskObservation> {
        RepairTaskObservation::new(
            match error {
                RepairTaskFailure::AccessDenied => RepairDiagnostic::AccessDenied,
                RepairTaskFailure::Native(error) => diagnostic(error),
            },
            None,
        )
    }
    pub(crate) fn probe(
        io: Arc<WindowsNativeIo>,
        desired: Definition,
        deadline: &Deadline,
    ) -> NativeResult<RepairTaskObservation> {
        let budget = deadline.clone();
        PROBE.get_or_init(|| Arc::new(CallOwner::default())).run(
            Dispatch::Observation,
            deadline,
            move || {
                let check = || io.admit_support(&budget).map(|_| ());
                let scheduler = match Scheduler::connect_repair(&check) {
                    Ok(scheduler) => scheduler,
                    Err(error) => return failed(error),
                };
                let observed = match scheduler.repair_snapshot(&check) {
                    Ok(observed) => observed,
                    Err(error) => return failed(error),
                };
                // Actual disabled state is preserved BEFORE independent template or role matching.
                if observed.as_ref().is_some_and(|task| !task.enabled) {
                    return RepairTaskObservation::new(RepairDiagnostic::Disabled, None);
                }
                let expected = match scheduler.repair_expected(&desired, &check) {
                    Ok(expected) => expected,
                    Err(error) => return RepairTaskObservation::new(diagnostic(error), None),
                };
                match observed {
                    None => RepairTaskObservation::new(RepairDiagnostic::Missing, Some(expected)),
                    Some(task) => match scheduler.repair_whole_xml(&task.xml, &check) {
                        Ok(actual) if actual == expected => {
                            RepairTaskObservation::new(RepairDiagnostic::Healthy, Some(expected))
                        }
                        // Enabled whole-XML drift is Unknown, never a registration plan.
                        _ => RepairTaskObservation::new(RepairDiagnostic::Unknown, None),
                    },
                }
            },
        )
    }
    pub(crate) fn register(
        binding: Arc<RepairTaskBinding>,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        binding.check(deadline)?;
        let retained = binding.clone();
        let budget = deadline.clone();
        let result = REGISTER.get_or_init(|| Arc::new(CallOwner::default())).run(
            Dispatch::Mutation,
            deadline,
            move || {
                binding.finish((|| {
                    let check = || binding.check(&budget);
                    let mut scheduler =
                        Scheduler::connect_repair(&check).map_err(|error| match error {
                            RepairTaskFailure::AccessDenied => NativeError::Foreign,
                            RepairTaskFailure::Native(error) => error,
                        })?;
                    scheduler.register_missing_repair(
                        binding.desired(),
                        binding.task_xml()?,
                        &check,
                        &|| binding.reached(),
                    )?;
                    binding.check(&budget)
                })())
            },
        );
        if matches!(result, Err(NativeError::OutcomeUnknown)) {
            retained.retire();
            let _ = UNCERTAIN.set(retained);
        }
        result
    }
}
#[cfg(all(windows, not(test)))]
pub(crate) use repair_native::{probe as repair_probe, register as repair_register};
