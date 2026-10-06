//! Bounded task metadata comparison and fakeable dispatch ordering, never executable approval.

use super::super::native_io::{NativeError, NativeResult};

pub const TASK_NAME: &str = "\\Crosspane\\Agent";
pub const SUPERVISOR_ARGUMENT: &str = "--windows-supervisor";
pub const MAX_XML_BYTES: usize = 64 * 1024;

/// Lead750c0da2: A4 alone supplies the fixed-role identity/pins constructor. This adapter
/// consumes the opaque prerequisite unchanged; COM registration/start is held until then.
pub(super) fn native_start(_trusted: &super::TrustedImages) -> NativeResult<()> {
    Err(NativeError::Unsupported)
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
