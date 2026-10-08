//! Public Task Scheduler COM adapter. All interfaces live and die on one owned MTA thread.
#[cfg(all(windows, not(test)))]
mod native {
    use super::super::super::service::task::{
        Definition, Logon, MAX_XML_BYTES, RunLevel, SUPERVISOR_ARGUMENT, Snapshot, TASK_NAME,
    };
    use super::super::{
        NativeError, NativeResult,
        activation::{RegistrationStep, TaskSubmission},
    };
    use std::{marker::PhantomData, rc::Rc, sync::OnceLock};
    use windows::{
        Win32::{
            Foundation::{RPC_E_TOO_LATE, VARIANT_BOOL},
            System::{Com::*, TaskScheduler::*, Variant::VARIANT},
        },
        core::{BSTR, Interface},
    };

    macro_rules! call {
        ($check:expr, $expr:expr) => {{
            ($check)()?;
            // SAFETY: the adapter owns this MTA's interfaces and valid bounded inputs/output slots.
            // The admission closure renews the original context, image pins and deadline first.
            unsafe { $expr }.map_err(|_| NativeError::Unavailable)
        }};
    }
    static SECURITY: OnceLock<NativeResult<()>> = OnceLock::new();
    struct Apartment(PhantomData<Rc<()>>);
    impl Apartment {
        fn new(check: &dyn Fn() -> NativeResult<()>) -> NativeResult<Self> {
            check()?;
            // SAFETY: this newly owned call worker initializes only its own MTA apartment.
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                .ok()
                .map_err(|_| NativeError::Unsupported)?;
            let apartment = Self(PhantomData);
            (*SECURITY.get_or_init(|| {
                check()?;
                // SAFETY: process-once standard COM security, no custom descriptor or borrowed list.
                unsafe {
                    CoInitializeSecurity(
                        None,
                        -1,
                        None,
                        None,
                        RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
                        RPC_C_IMP_LEVEL_IMPERSONATE,
                        None,
                        EOAC_NONE,
                        None,
                    )
                }
                .map_err(|error| {
                    if error.code() == RPC_E_TOO_LATE {
                        NativeError::Unsupported
                    } else {
                        NativeError::Unavailable
                    }
                })
            }))?;
            Ok(apartment)
        }
    }
    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: balances successful S_OK/S_FALSE initialization on this exact worker thread.
            unsafe {
                CoUninitialize();
            }
        }
    }
    pub(crate) struct Scheduler {
        service: ITaskService,
        root: ITaskFolder,
        folder: Option<ITaskFolder>,
        // Declared LAST: COM references are released before apartment uninitialization.
        _apartment: Apartment,
    }
    pub(crate) fn bounded_bstr(value: &BSTR, cap: usize) -> NativeResult<String> {
        if value.len() > cap {
            return Err(NativeError::Oversize);
        }
        let mut output = String::new();
        for character in char::decode_utf16(value.iter().copied()) {
            let character = character.map_err(|_| NativeError::Invalid)?;
            if character == '\0' {
                return Err(NativeError::Invalid);
            }
            if output.len().saturating_add(character.len_utf8()) > cap {
                return Err(NativeError::Oversize);
            }
            output.push(character);
        }
        Ok(output)
    }
    fn missing(error: &windows::core::Error) -> bool {
        error.code().0 as u32 == 0x80070002
    }
    /// Read-only repair facts. XML/Enabled never grant registration or execution authority.
    pub(crate) struct RepairTaskSnapshot {
        pub(crate) xml: String,
        pub(crate) enabled: bool,
    }
    pub(crate) enum RepairTaskFailure {
        AccessDenied,
        Native(NativeError),
    }
    impl From<NativeError> for RepairTaskFailure {
        fn from(error: NativeError) -> Self {
            Self::Native(error)
        }
    }
    fn repair_com<T>(result: windows::core::Result<T>) -> Result<T, RepairTaskFailure> {
        result.map_err(|error| {
            if error.code().0 as u32 == 0x80070005 {
                RepairTaskFailure::AccessDenied
            } else {
                RepairTaskFailure::Native(NativeError::Unavailable)
            }
        })
    }
    macro_rules! repair_call {
        ($check:expr, $expr:expr) => {{
            ($check)().map_err(RepairTaskFailure::from)?;
            // SAFETY: this owned MTA retains all COM references and bounded output slots;
            // the genuine read-only admission closure precedes every exact SDK call.
            repair_com(unsafe { $expr })
        }};
    }
    impl Scheduler {
        pub(crate) fn connect(check: &dyn Fn() -> NativeResult<()>) -> NativeResult<Self> {
            let apartment = Apartment::new(check)?;
            let service: ITaskService = call!(
                check,
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)
            )?;
            let empty = VARIANT::default();
            call!(check, service.Connect(&empty, &empty, &empty, &empty))?;
            let root = call!(check, service.GetFolder(&BSTR::from("\\")))?;
            check()?;
            // SAFETY: fixed local namespace, on the same owned apartment; only not-found is absence.
            let folder = match unsafe { service.GetFolder(&BSTR::from("\\Crosspane")) } {
                Ok(folder) => Some(folder),
                Err(error) if missing(&error) => None,
                Err(_) => return Err(NativeError::Unavailable),
            };
            Ok(Self {
                service,
                root,
                folder,
                _apartment: apartment,
            })
        }
        /// Separate diagnosis preserves the actual SDK denial before the old error mapping.
        /// It creates no folder/task, and leaves the original connect constructor unchanged.
        pub(crate) fn connect_repair(
            check: &dyn Fn() -> NativeResult<()>,
        ) -> Result<Self, RepairTaskFailure> {
            let apartment = Apartment::new(check)?;
            let service: ITaskService = repair_call!(
                check,
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)
            )?;
            let empty = VARIANT::default();
            repair_call!(check, service.Connect(&empty, &empty, &empty, &empty))?;
            let root = repair_call!(check, service.GetFolder(&BSTR::from("\\")))?;
            // Repair snapshots always re-read the fixed folder, rather than caching absence.
            Ok(Self {
                service,
                root,
                folder: None,
                _apartment: apartment,
            })
        }
        /// Read Xml and Enabled before any role/property checks, so disabled stays preserved.
        /// No task enumeration, property-subset approval or XML rewriting occurs here.
        pub(crate) fn repair_snapshot(
            &self,
            check: &dyn Fn() -> NativeResult<()>,
        ) -> Result<Option<RepairTaskSnapshot>, RepairTaskFailure> {
            check()?;
            // SAFETY: fixed local folder on this owned MTA; only exact not-found is absence.
            let folder = match unsafe { self.service.GetFolder(&BSTR::from("\\Crosspane")) } {
                Ok(folder) => folder,
                Err(error) if missing(&error) => return Ok(None),
                Err(error) => return repair_com::<ITaskFolder>(Err(error)).map(|_| None),
            };
            check()?;
            // SAFETY: fixed Agent leaf on this owned MTA, no alternative lookup.
            let task = match unsafe { folder.GetTask(&BSTR::from("Agent")) } {
                Ok(task) => task,
                Err(error) if missing(&error) => return Ok(None),
                Err(error) => return repair_com::<IRegisteredTask>(Err(error)).map(|_| None),
            };
            let enabled = repair_call!(check, task.Enabled())?.0 != 0;
            // Proven user-disabled state is preserved even if Xml would be denied/malformed.
            // Empty XML is a private disabled sentinel, never an approval/comparison input.
            if !enabled {
                return Ok(Some(RepairTaskSnapshot {
                    xml: String::new(),
                    enabled,
                }));
            }
            let xml = bounded_bstr(&repair_call!(check, task.Xml())?, MAX_XML_BYTES)?;
            if xml.is_empty() {
                return Err(NativeError::Invalid.into());
            }
            check()?;
            Ok(Some(RepairTaskSnapshot { xml, enabled }))
        }
        /// Independent whole-XML SDK form; no observed fields are copied into the expectation.
        pub(crate) fn repair_expected(
            &self,
            desired: &Definition,
            check: &dyn Fn() -> NativeResult<()>,
        ) -> NativeResult<String> {
            self.removal_canonical(&self.removal_template(desired, check)?, check)
        }
        pub(crate) fn repair_whole_xml(
            &self,
            xml: &str,
            check: &dyn Fn() -> NativeResult<()>,
        ) -> NativeResult<String> {
            self.removal_canonical(xml, check)
        }
        /// Register ONLY a positively absent fixed task. Never Update, Enable an existing task,
        /// or Run. The durable repair intent and genuine current image/agent pins come from caller.
        pub(crate) fn register_missing_repair(
            &mut self,
            desired: &Definition,
            expected: &str,
            check: &dyn Fn() -> NativeResult<()>,
            effect: &dyn Fn(),
        ) -> NativeResult<()> {
            if self.repair_expected(desired, check)? != expected {
                return Err(NativeError::Foreign);
            }
            match self.repair_snapshot(check) {
                Ok(None) => {}
                Ok(Some(_)) => return Err(NativeError::Foreign),
                Err(RepairTaskFailure::Native(error)) => return Err(error),
                Err(RepairTaskFailure::AccessDenied) => return Err(NativeError::Foreign),
            }
            let definition = call!(check, self.service.NewTask(0))?;
            call!(check, definition.SetXmlText(&BSTR::from(expected)))?;
            check()?;
            // SAFETY: fixed local folder read only; no unrelated task namespace selection.
            let folder = match unsafe { self.service.GetFolder(&BSTR::from("\\Crosspane")) } {
                Ok(folder) => folder,
                Err(error) if missing(&error) => {
                    check()?;
                    effect();
                    // SAFETY: only this fixed folder, after durable intent and fresh admission.
                    unsafe {
                        self.root
                            .CreateFolder(&BSTR::from("Crosspane"), &VARIANT::default())
                    }
                    .map_err(|_| NativeError::OutcomeUnknown)?
                }
                Err(_) => return Err(NativeError::Unavailable),
            };
            // Reobserve immediately adjacent to TASK_CREATE. CREATE refuses an intervening task;
            // it never overwrites a newly disabled or modified definition.
            check()?;
            // SAFETY: exact fixed leaf, read only on this owned MTA.
            match unsafe { folder.GetTask(&BSTR::from("Agent")) } {
                Err(error) if missing(&error) => {}
                _ => return Err(NativeError::Foreign),
            }
            check()?;
            effect();
            // SAFETY: exact Limited/current-user logon-only template. CREATE, never UPDATE;
            // registration-trigger suppression does not invoke Run and no time trigger exists.
            let _registered = unsafe {
                folder.RegisterTaskDefinition(
                    &BSTR::from("Agent"),
                    &definition,
                    TASK_CREATE.0 | TASK_IGNORE_REGISTRATION_TRIGGERS.0,
                    &VARIANT::from(desired.principal.as_str()),
                    &VARIANT::default(),
                    TASK_LOGON_INTERACTIVE_TOKEN,
                    &VARIANT::default(),
                )
            }
            .map_err(|_| NativeError::OutcomeUnknown)?;
            check()?;
            // SDK automatic registration fields may differ: retain that mismatch as Unknown;
            // never normalize/copy them or claim exact native template equivalence.
            let actual = match self.repair_snapshot(check) {
                Ok(Some(actual)) => actual,
                _ => return Err(NativeError::OutcomeUnknown),
            };
            if !actual.enabled || self.repair_whole_xml(&actual.xml, check)? != expected {
                return Err(NativeError::OutcomeUnknown);
            }
            check()
        }
        pub(crate) fn inspect(
            &self,
            check: &dyn Fn() -> NativeResult<()>,
        ) -> NativeResult<Option<Snapshot>> {
            let Some(folder) = &self.folder else {
                return Ok(None);
            };
            check()?;
            // SAFETY: exact fixed leaf; no task enumeration or alternate name selection.
            let task = match unsafe { folder.GetTask(&BSTR::from("Agent")) } {
                Ok(task) => task,
                Err(error) if missing(&error) => return Ok(None),
                Err(_) => return Err(NativeError::Unavailable),
            };
            let xml = bounded_bstr(&call!(check, task.Xml())?, MAX_XML_BYTES)?;
            if xml.is_empty() {
                return Err(NativeError::Foreign);
            }
            let path = bounded_bstr(&call!(check, task.Path())?, 256)?;
            if path != TASK_NAME {
                return Err(NativeError::Foreign);
            }
            let definition = call!(check, task.Definition())?;
            let principal = call!(check, definition.Principal())?;
            let mut user = BSTR::new();
            let mut group = BSTR::new();
            let mut logon = TASK_LOGON_NONE;
            let mut level = TASK_RUNLEVEL_HIGHEST;
            call!(check, principal.UserId(&mut user))?;
            call!(check, principal.GroupId(&mut group))?;
            call!(check, principal.LogonType(&mut logon))?;
            call!(check, principal.RunLevel(&mut level))?;
            if !group.is_empty()
                || logon != TASK_LOGON_INTERACTIVE_TOKEN
                || level != TASK_RUNLEVEL_LUA
            {
                return Err(NativeError::Foreign);
            }
            let triggers = call!(check, definition.Triggers())?;
            let mut count = 0;
            call!(check, triggers.Count(&mut count))?;
            if count != 1 {
                return Err(NativeError::Foreign);
            }
            let trigger = call!(check, triggers.get_Item(1))?;
            let mut kind = TASK_TRIGGER_EVENT;
            call!(check, trigger.Type(&mut kind))?;
            if kind != TASK_TRIGGER_LOGON {
                return Err(NativeError::Foreign);
            }
            let repetition = call!(check, trigger.Repetition())?;
            let mut interval = BSTR::new();
            let mut duration = BSTR::new();
            let mut start_boundary = BSTR::new();
            let mut end_boundary = BSTR::new();
            let mut trigger_enabled = VARIANT_BOOL(0);
            call!(check, repetition.Interval(&mut interval))?;
            call!(check, repetition.Duration(&mut duration))?;
            call!(check, trigger.StartBoundary(&mut start_boundary))?;
            call!(check, trigger.EndBoundary(&mut end_boundary))?;
            call!(check, trigger.Enabled(&mut trigger_enabled))?;
            if !interval.is_empty()
                || !duration.is_empty()
                || !start_boundary.is_empty()
                || !end_boundary.is_empty()
                || trigger_enabled.0 == 0
            {
                return Err(NativeError::Foreign);
            }
            let trigger: ILogonTrigger = trigger.cast().map_err(|_| NativeError::Foreign)?;
            let mut trigger_user = BSTR::new();
            let mut delay = BSTR::new();
            call!(check, trigger.UserId(&mut trigger_user))?;
            call!(check, trigger.Delay(&mut delay))?;
            if !delay.is_empty() {
                return Err(NativeError::Foreign);
            }
            let actions = call!(check, definition.Actions())?;
            call!(check, actions.Count(&mut count))?;
            if count != 1 {
                return Err(NativeError::Foreign);
            }
            let action = call!(check, actions.get_Item(1))?;
            let mut kind = TASK_ACTION_COM_HANDLER;
            call!(check, action.Type(&mut kind))?;
            if kind != TASK_ACTION_EXEC {
                return Err(NativeError::Foreign);
            }
            let action: IExecAction = action.cast().map_err(|_| NativeError::Foreign)?;
            let mut action_path = BSTR::new();
            let mut arguments = BSTR::new();
            let mut working = BSTR::new();
            call!(check, action.Path(&mut action_path))?;
            call!(check, action.Arguments(&mut arguments))?;
            call!(check, action.WorkingDirectory(&mut working))?;
            let settings = call!(check, definition.Settings())?;
            let mut policy = TASK_INSTANCES_PARALLEL;
            let mut restart = 0;
            call!(check, settings.MultipleInstances(&mut policy))?;
            call!(check, settings.RestartCount(&mut restart))?;
            if restart < 0 {
                return Err(NativeError::Foreign);
            }
            let mut demand = VARIANT_BOOL(0);
            let mut stop_battery = VARIANT_BOOL(0);
            let mut no_battery = VARIANT_BOOL(0);
            let mut execution = BSTR::new();
            call!(check, settings.AllowDemandStart(&mut demand))?;
            call!(check, settings.StopIfGoingOnBatteries(&mut stop_battery))?;
            call!(check, settings.DisallowStartIfOnBatteries(&mut no_battery))?;
            call!(check, settings.ExecutionTimeLimit(&mut execution))?;
            let mut hard = VARIANT_BOOL(0);
            let mut idle = VARIANT_BOOL(0);
            let mut network = VARIANT_BOOL(0);
            let mut wake = VARIANT_BOOL(0);
            call!(check, settings.AllowHardTerminate(&mut hard))?;
            call!(check, settings.RunOnlyIfIdle(&mut idle))?;
            call!(check, settings.RunOnlyIfNetworkAvailable(&mut network))?;
            call!(check, settings.WakeToRun(&mut wake))?;
            if demand.0 == 0
                || stop_battery.0 != 0
                || no_battery.0 != 0
                || hard.0 != 0
                || idle.0 != 0
                || network.0 != 0
                || wake.0 != 0
                || bounded_bstr(&execution, 32)? != "PT0S"
            {
                return Err(NativeError::Foreign);
            }
            Ok(Some(Snapshot {
                xml,
                definition: Definition {
                    name: path,
                    principal: bounded_bstr(&user, 256)?,
                    trigger_user: bounded_bstr(&trigger_user, 256)?,
                    logon: Logon::InteractiveToken,
                    run_level: RunLevel::Limited,
                    action: bounded_bstr(&action_path, 32768)?,
                    arguments: bounded_bstr(&arguments, 256)?,
                    working_directory: bounded_bstr(&working, 32768)?,
                    logon_trigger_only: true,
                    ignore_new_instance: policy == TASK_INSTANCES_IGNORE_NEW,
                    manager_restart_count: restart as u32,
                    enabled: call!(check, task.Enabled())?.0 != 0,
                },
            }))
        }
        /// Caller durably published exact original XML before this first external namespace mutation.
        pub(crate) fn register(
            &mut self,
            desired: &Definition,
            existing: bool,
            check: &dyn Fn() -> NativeResult<()>,
            journal: &mut dyn FnMut(RegistrationStep) -> NativeResult<()>,
        ) -> NativeResult<()> {
            if self.folder.is_none() {
                journal(RegistrationStep::FolderIntent)?;
                let folder = call!(
                    check,
                    self.root
                        .CreateFolder(&BSTR::from("Crosspane"), &VARIANT::default())
                )?;
                self.folder = Some(folder);
                journal(RegistrationStep::FolderCreated)?;
            }
            let definition = call!(check, self.service.NewTask(0))?;
            let principal = call!(check, definition.Principal())?;
            call!(
                check,
                principal.SetUserId(&BSTR::from(desired.principal.as_str()))
            )?;
            call!(check, principal.SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN))?;
            call!(check, principal.SetRunLevel(TASK_RUNLEVEL_LUA))?;
            let triggers = call!(check, definition.Triggers())?;
            let trigger = call!(check, triggers.Create(TASK_TRIGGER_LOGON))?;
            let trigger: ILogonTrigger = trigger.cast().map_err(|_| NativeError::Unavailable)?;
            call!(
                check,
                trigger.SetUserId(&BSTR::from(desired.trigger_user.as_str()))
            )?;
            let actions = call!(check, definition.Actions())?;
            let action = call!(check, actions.Create(TASK_ACTION_EXEC))?;
            let action: IExecAction = action.cast().map_err(|_| NativeError::Unavailable)?;
            call!(check, action.SetPath(&BSTR::from(desired.action.as_str())))?;
            call!(check, action.SetArguments(&BSTR::from(SUPERVISOR_ARGUMENT)))?;
            call!(
                check,
                action.SetWorkingDirectory(&BSTR::from(desired.working_directory.as_str()))
            )?;
            let settings = call!(check, definition.Settings())?;
            call!(
                check,
                settings.SetMultipleInstances(TASK_INSTANCES_IGNORE_NEW)
            )?;
            call!(check, settings.SetRestartCount(0))?;
            call!(check, settings.SetAllowDemandStart(VARIANT_BOOL(-1)))?;
            call!(check, settings.SetExecutionTimeLimit(&BSTR::from("PT0S")))?;
            call!(check, settings.SetStopIfGoingOnBatteries(VARIANT_BOOL(0)))?;
            call!(
                check,
                settings.SetDisallowStartIfOnBatteries(VARIANT_BOOL(0))
            )?;
            call!(check, settings.SetAllowHardTerminate(VARIANT_BOOL(0)))?;
            call!(check, settings.SetRunOnlyIfIdle(VARIANT_BOOL(0)))?;
            call!(
                check,
                settings.SetRunOnlyIfNetworkAvailable(VARIANT_BOOL(0))
            )?;
            call!(check, settings.SetWakeToRun(VARIANT_BOOL(0)))?;
            call!(check, settings.SetEnabled(VARIANT_BOOL(-1)))?;
            let flags = (if existing {
                TASK_UPDATE.0
            } else {
                TASK_CREATE.0
            }) | TASK_IGNORE_REGISTRATION_TRIGGERS.0;
            let folder = self.folder.as_ref().ok_or(NativeError::Unavailable)?;
            journal(RegistrationStep::DefinitionIntent)?;
            let _registered = call!(
                check,
                folder.RegisterTaskDefinition(
                    &BSTR::from("Agent"),
                    &definition,
                    flags,
                    &VARIANT::from(desired.principal.as_str()),
                    &VARIANT::default(),
                    TASK_LOGON_INTERACTIVE_TOKEN,
                    &VARIANT::default()
                )
            )?;
            journal(RegistrationStep::DefinitionRegistered)?;
            Ok(())
        }
        /// Generated expectation only. SDK roundtripping does not promise that registration's
        /// automatic author/date fields match this template. Any discrepancy is retained, never
        /// copied from the observed task or normalized away. Native equivalence remains unproven.
        pub(crate) fn removal_template(
            &self,
            desired: &Definition,
            check: &dyn Fn() -> NativeResult<()>,
        ) -> NativeResult<String> {
            let definition = call!(check, self.service.NewTask(0))?;
            let principal = call!(check, definition.Principal())?;
            call!(
                check,
                principal.SetUserId(&BSTR::from(desired.principal.as_str()))
            )?;
            call!(check, principal.SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN))?;
            call!(check, principal.SetRunLevel(TASK_RUNLEVEL_LUA))?;
            let triggers = call!(check, definition.Triggers())?;
            let trigger = call!(check, triggers.Create(TASK_TRIGGER_LOGON))?;
            let trigger: ILogonTrigger = trigger.cast().map_err(|_| NativeError::Unavailable)?;
            call!(
                check,
                trigger.SetUserId(&BSTR::from(desired.trigger_user.as_str()))
            )?;
            let actions = call!(check, definition.Actions())?;
            let action = call!(check, actions.Create(TASK_ACTION_EXEC))?;
            let action: IExecAction = action.cast().map_err(|_| NativeError::Unavailable)?;
            call!(check, action.SetPath(&BSTR::from(desired.action.as_str())))?;
            call!(check, action.SetArguments(&BSTR::from(SUPERVISOR_ARGUMENT)))?;
            call!(
                check,
                action.SetWorkingDirectory(&BSTR::from(desired.working_directory.as_str()))
            )?;
            let settings = call!(check, definition.Settings())?;
            call!(
                check,
                settings.SetMultipleInstances(TASK_INSTANCES_IGNORE_NEW)
            )?;
            call!(check, settings.SetRestartCount(0))?;
            call!(check, settings.SetAllowDemandStart(VARIANT_BOOL(-1)))?;
            call!(check, settings.SetExecutionTimeLimit(&BSTR::from("PT0S")))?;
            call!(check, settings.SetStopIfGoingOnBatteries(VARIANT_BOOL(0)))?;
            call!(
                check,
                settings.SetDisallowStartIfOnBatteries(VARIANT_BOOL(0))
            )?;
            call!(check, settings.SetAllowHardTerminate(VARIANT_BOOL(0)))?;
            call!(check, settings.SetRunOnlyIfIdle(VARIANT_BOOL(0)))?;
            call!(
                check,
                settings.SetRunOnlyIfNetworkAvailable(VARIANT_BOOL(0))
            )?;
            call!(check, settings.SetWakeToRun(VARIANT_BOOL(0)))?;
            call!(check, settings.SetEnabled(VARIANT_BOOL(-1)))?;
            let information = call!(check, definition.RegistrationInfo())?;
            call!(check, information.SetURI(&BSTR::from(TASK_NAME)))?;
            let mut xml = BSTR::new();
            call!(check, definition.XmlText(&mut xml))?;
            bounded_bstr(&xml, MAX_XML_BYTES)
        }
        fn removal_canonical(
            &self,
            xml: &str,
            check: &dyn Fn() -> NativeResult<()>,
        ) -> NativeResult<String> {
            if xml.is_empty() || xml.len() > MAX_XML_BYTES || xml.contains('\0') {
                return Err(NativeError::Invalid);
            }
            let definition = call!(check, self.service.NewTask(0))?;
            call!(check, definition.SetXmlText(&BSTR::from(xml)))?;
            let mut xml = BSTR::new();
            call!(check, definition.XmlText(&mut xml))?;
            bounded_bstr(&xml, MAX_XML_BYTES)
        }
        /// Exact fixed task only. Read-only admission compares the WHOLE bounded SDK XML form;
        /// no property subset, observed author/date copying, enumeration or path alias is accepted.
        pub(crate) fn inspect_removal(
            &self,
            desired: &Definition,
            check: &dyn Fn() -> NativeResult<()>,
        ) -> NativeResult<(String, Option<String>)> {
            let expected =
                self.removal_canonical(&self.removal_template(desired, check)?, check)?;
            let observed = self
                .inspect(check)?
                .map(|snapshot| self.removal_canonical(&snapshot.xml, check))
                .transpose()?;
            if observed.as_deref().is_some_and(|xml| xml != expected) {
                return Err(NativeError::Foreign);
            }
            check()?;
            Ok((expected, observed))
        }
        /// Intent and genuine completed-tree admission precede entry. The public SDK has no
        /// compare-and-delete: fresh whole XML equality is adjacent to DeleteTask, then fixed
        /// absence is checked. Concurrent task edits/unknown outcomes are conservatively retained.
        pub(crate) fn delete_removal(
            &self,
            desired: &Definition,
            expected: &str,
            check: &dyn Fn() -> NativeResult<()>,
            effect: &dyn Fn(),
        ) -> NativeResult<()> {
            let (fresh, observed) = self.inspect_removal(desired, check)?;
            if fresh != expected {
                return Err(NativeError::Foreign);
            }
            if observed.is_none() {
                return Ok(());
            }
            let folder = self.folder.as_ref().ok_or(NativeError::Foreign)?;
            check()?;
            // Record may-have-mutated immediately before the actual COM deletion dispatch.
            effect();
            // SAFETY: exact fixed task name after fresh whole-XML admission on this owned MTA.
            // DeleteTask offers no atomic expected-XML argument; flags 0 never widen the target.
            unsafe { folder.DeleteTask(&BSTR::from("Agent"), 0) }
                .map_err(|_| NativeError::OutcomeUnknown)?;
            check()?;
            // SAFETY: read-only fixed leaf absence check on the same MTA, no folder enumeration.
            let observed = unsafe { folder.GetTask(&BSTR::from("Agent")) };
            check()?;
            match observed {
                Err(error) if missing(&error) => Ok(()),
                _ => Err(NativeError::OutcomeUnknown),
            }
        }
        /// F1 cold partial cleanup only: callers hold the real first-recovery reservation and
        /// durable TaskDeleteIntent. This cannot consume a completed-tree removal capability.
        pub(crate) fn delete_first_partial(
            &self,
            desired: &Definition,
            expected: &str,
            check: &dyn Fn() -> NativeResult<()>,
            effect: &dyn Fn(),
        ) -> NativeResult<()> {
            let (fresh, observed) = self.inspect_removal(desired, check)?;
            if fresh != expected {
                return Err(NativeError::Foreign);
            }
            if observed.is_none() {
                return Ok(());
            }
            let folder = self.folder.as_ref().ok_or(NativeError::Foreign)?;
            check()?;
            // Record may-have-mutated immediately before the actual COM deletion dispatch.
            effect();
            // SAFETY: exact fixed task name after fresh whole-XML admission on this owned MTA.
            // DeleteTask offers no atomic expected-XML argument; flags 0 never widen the target.
            unsafe { folder.DeleteTask(&BSTR::from("Agent"), 0) }
                .map_err(|_| NativeError::OutcomeUnknown)?;
            check()?;
            // SAFETY: read-only fixed leaf absence check on the same MTA, no folder enumeration.
            let observed = unsafe { folder.GetTask(&BSTR::from("Agent")) };
            check()?;
            match observed {
                Err(error) if missing(&error) => Ok(()),
                _ => Err(NativeError::OutcomeUnknown),
            }
        }
        pub(crate) fn run(
            &self,
            check: &dyn Fn() -> NativeResult<()>,
        ) -> NativeResult<TaskSubmission> {
            let folder = self.folder.as_ref().ok_or(NativeError::Missing)?;
            let task = call!(check, folder.GetTask(&BSTR::from("Agent")))?;
            let running = call!(check, task.Run(&VARIANT::default()))?;
            let guid = bounded_bstr(&call!(check, running.InstanceGuid())?, 38)?;
            TaskSubmission::new(guid)
        }
    }
}
#[cfg(all(windows, not(test)))]
pub(crate) use native::{RepairTaskFailure, Scheduler};
