//! The `\Crosspane\Agent` sign-in task (L7): XML, registration, inspection, stop, run and delete.
//!
//! Every function that talks to Task Scheduler does its COM work on its own short-lived thread,
//! inside its own `CoInitializeEx(COINIT_MULTITHREADED)` scope, so the GUI thread's apartment is
//! never involved. All interfaces are released before `CoUninitialize`, and a COM failure is never
//! remembered: the next call starts from scratch.

use super::{
    error::{SetupError, SetupResult},
    identity::native as identity,
    paths::{Paths, PayloadFile, same_path},
    setup_log::setup_note,
    supervisor::{SUPERVISOR_ALIAS, SUPERVISOR_FLAG},
};
use std::{
    marker::PhantomData,
    path::Path,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{RPC_E_TOO_LATE, VARIANT_BOOL},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoInitializeSecurity, CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
                RPC_C_IMP_LEVEL_IMPERSONATE,
            },
            TaskScheduler::{
                IExecAction, ILogonTrigger, IRegisteredTask, ITaskDefinition, ITaskFolder,
                ITaskService, TASK_ACTION_EXEC, TASK_ACTION_TYPE, TASK_CREATE_OR_UPDATE,
                TASK_ENUM_HIDDEN, TASK_INSTANCES_IGNORE_NEW, TASK_INSTANCES_POLICY,
                TASK_LOGON_INTERACTIVE_TOKEN, TASK_LOGON_TYPE, TASK_RUNLEVEL_LUA,
                TASK_RUNLEVEL_TYPE, TASK_STATE_RUNNING, TASK_TRIGGER_LOGON, TASK_TRIGGER_TYPE2,
                TaskScheduler,
            },
            Variant::VARIANT,
        },
    },
    core::{BSTR, Interface},
};

/// One COM call on an interface this thread created, as a `Result<_, ComError>`. Defined first:
/// `macro_rules!` macros are visible only below their definition.
macro_rules! com {
    ($call:expr) => {{
        // SAFETY: every call goes through an interface created on this thread inside its own MTA
        // scope, with arguments that outlive the call and bounded out slots; nothing is retained.
        let result = unsafe { $call };
        logged(result)
    }};
}

pub(crate) const TASK_PATH: &str = r"\Crosspane\Agent";

/// The task's folder, with and without the leading separator, and the task's name inside it.
const FOLDER_PATH: &str = r"\Crosspane";
const FOLDER_NAME: &str = "Crosspane";
const LEAF: &str = "Agent";

/// How long `stop_instances` waits for the task to leave `Running`, and how often it looks.
const STOP_WAIT: Duration = Duration::from_secs(5);
const STOP_POLL: Duration = Duration::from_millis(200);

/// Upper bounds for values read back from Task Scheduler.
const MAX_ITEMS: i32 = 16;
const MAX_ACCOUNT_CHARS: usize = 256;
const MAX_PATH_CHARS: usize = 32768;
const MAX_ARGUMENT_CHARS: usize = 1024;
const MAX_LIMIT_CHARS: usize = 32;

/// What Task Scheduler says about the sign-in task, compared field by field (never as XML text).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskHealth {
    Healthy,
    Missing,
    Disabled,
    Different,
    Unavailable,
}

/// Pure. XML-escaped; logon trigger + principal by SID, InteractiveToken, LeastPrivilege,
/// IgnoreNew, PT0S, battery false/false, AllowHardTerminate true, Priority 4, Exec
/// `"<install>\crosspane-installer.exe" --windows-supervisor`, WorkingDirectory `<install>`.
pub(crate) fn task_xml(paths: &Paths, user_sid: &str) -> String {
    let sid = escape(user_sid);
    let installer = escape(&plain(&paths.file(PayloadFile::Installer)));
    let install = escape(&plain(&paths.install));
    let arguments = escape(SUPERVISOR_FLAG);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Author>Crosspane</Author><Description>Starts Crosspane when you sign in.</Description><URI>\Crosspane\Agent</URI></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>true</AllowHardTerminate><StartWhenAvailable>false</StartWhenAvailable><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><Hidden>false</Hidden><RunOnlyIfIdle>false</RunOnlyIfIdle><WakeToRun>false</WakeToRun><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Priority>4</Priority></Settings>
  <Actions Context="Author"><Exec><Command>{installer}</Command><Arguments>{arguments}</Arguments><WorkingDirectory>{install}</WorkingDirectory></Exec></Actions>
</Task>
"#
    )
}

/// Healthy = exists, enabled, one Exec whose path `same_path(installer)` and arguments ==
/// `--windows-supervisor`, one LOGON trigger whose user ∈ {SID, `account_name` (DOMAIN\name), bare
/// name}, plus the principal and the settings that decide whether the supervisor starts at all.
/// Never compares XML text.
pub(crate) fn inspect(paths: &Paths) -> TaskHealth {
    let Some(own) = Own::current() else {
        setup_note!("task inspect: the signed-in account could not be read");
        return TaskHealth::Unavailable;
    };
    let installer = paths.file(PayloadFile::Installer);
    match on_com_thread(|scheduler| health(scheduler, &installer, &own)) {
        Ok(health) => health,
        Err(error) => {
            setup_note!("task inspect failed: com error 0x{:08X}", error.0);
            TaskHealth::Unavailable
        }
    }
}

/// Folder create-if-missing; `RegisterTask(TASK_CREATE_OR_UPDATE, VARIANT(SID),
/// TASK_LOGON_INTERACTIVE_TOKEN)`.
pub(crate) fn register(paths: &Paths) -> SetupResult<()> {
    let signed_in = identity::current().map_err(|_| {
        SetupError::before(
            "Crosspane Setup runs only in your own signed-in Windows session, without administrator rights.",
        )
    })?;
    let sid = signed_in.facts().user.sddl();
    let xml = task_xml(paths, &sid);
    on_com_thread(|scheduler| {
        let folder = scheduler.ensure_folder()?;
        let _registered = com!(folder.RegisterTask(
            &BSTR::from(LEAF),
            &BSTR::from(xml.as_str()),
            TASK_CREATE_OR_UPDATE.0,
            &VARIANT::from(sid.as_str()),
            &VARIANT::default(),
            TASK_LOGON_INTERACTIVE_TOKEN,
            &VARIANT::default(),
        ))?;
        Ok(())
    })
    .map_err(|error| {
        failed(
            "register",
            error,
            "Windows wouldn't set up Crosspane to start when you sign in.",
        )
    })
}

/// `IRegisteredTask::Stop(0)`; waits ≤5 s for State != Running; a missing task is Ok.
pub(crate) fn stop_instances() -> SetupResult<()> {
    let stopped = on_com_thread(|scheduler| {
        let Some(task) = scheduler.task()? else {
            return Ok(true);
        };
        match com!(task.Stop(0)) {
            Ok(()) => {}
            // Nothing was running, or the task vanished meanwhile: both mean there is nothing to stop.
            Err(error) if error.is_missing() || error.0 == ComError::NOT_RUNNING => {}
            Err(error) => return Err(error),
        }
        let deadline = Instant::now() + STOP_WAIT;
        loop {
            if com!(task.State())? != TASK_STATE_RUNNING {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(STOP_POLL);
        }
    })
    .map_err(|error| {
        failed(
            "stop",
            error,
            "Windows wouldn't stop Crosspane's sign-in task.",
        )
    })?;
    if stopped {
        Ok(())
    } else {
        setup_note!("task stop: still running after {} s", STOP_WAIT.as_secs());
        Err(SetupError::after(
            "Crosspane's sign-in task is still running after being asked to stop.",
        ))
    }
}

/// `IRegisteredTask::Run(VARIANT::default())`
pub(crate) fn run() -> SetupResult<()> {
    let started = on_com_thread(|scheduler| {
        let Some(task) = scheduler.task()? else {
            return Ok(false);
        };
        let _instance = com!(task.Run(&VARIANT::default()))?;
        Ok(true)
    })
    .map_err(|error| {
        failed(
            "run",
            error,
            "Windows wouldn't start Crosspane's sign-in task.",
        )
    })?;
    if started {
        Ok(())
    } else {
        Err(SetupError::after(
            "Crosspane isn't set up to start when you sign in yet, so Windows has nothing to start.",
        ))
    }
}

/// A missing task is Ok; `DeleteFolder("Crosspane")` best effort.
pub(crate) fn delete() -> SetupResult<()> {
    on_com_thread(|scheduler| {
        let Some(folder) = scheduler.folder(FOLDER_PATH)? else {
            return Ok(());
        };
        match com!(folder.DeleteTask(&BSTR::from(LEAF), 0)) {
            Ok(()) => {}
            Err(error) if error.is_missing() => {}
            Err(error) => return Err(error),
        }
        // Best effort. The folder goes only when nothing else lives in it; a failure here is ignored.
        let empty = folder_is_empty(&folder).unwrap_or(false);
        drop(folder);
        if empty {
            let _ = com!(scheduler.root.DeleteFolder(&BSTR::from(FOLDER_NAME), 0));
        }
        Ok(())
    })
    .map_err(|error| {
        failed(
            "delete",
            error,
            "Windows wouldn't remove Crosspane's sign-in task.",
        )
    })
}

// ---------------------------------------------------------------------------------------------
// Read-back
// ---------------------------------------------------------------------------------------------

/// The signed-in account in the three forms Task Scheduler may report it.
#[derive(Debug)]
struct Own {
    sid: String,
    /// `DOMAIN\name`, when Windows can name the account.
    account: Option<String>,
}

impl Own {
    fn current() -> Option<Self> {
        let facts = identity::observe().ok()?;
        Some(Self {
            sid: facts.user.sddl(),
            account: identity::account_name(&facts.user).ok(),
        })
    }

    fn names(&self, value: &str) -> bool {
        names_account(value, &self.sid, self.account.as_deref())
    }
}

/// Task Scheduler's COM API reports the principal as the bare account name and a logon trigger's
/// user as `DOMAIN\name`, even when both were registered by SID (the stored XML keeps the SID). A
/// value names this account when it is the SID, `DOMAIN\name`, or the bare name, ignoring case.
/// Any other value, including a different domain's account with the same bare name, does not.
fn names_account(value: &str, sid: &str, account: Option<&str>) -> bool {
    if value.is_empty() {
        return false;
    }
    if eq_ignore_case(value, sid) {
        return true;
    }
    let Some(account) = account else {
        return false;
    };
    if eq_ignore_case(value, account) {
        return true;
    }
    !value.contains('\\')
        && account
            .rsplit_once('\\')
            .is_some_and(|(_, bare)| !bare.is_empty() && eq_ignore_case(value, bare))
}

fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

/// Maps the live task onto a `TaskHealth`.
fn health(scheduler: &Scheduler, installer: &Path, own: &Own) -> Result<TaskHealth, ComError> {
    let Some(task) = scheduler.task()? else {
        return Ok(TaskHealth::Missing);
    };
    if com!(task.Enabled())?.0 == 0 {
        return Ok(TaskHealth::Disabled);
    }
    let definition = com!(task.Definition())?;
    Ok(match difference(&definition, installer, own)? {
        None => TaskHealth::Healthy,
        Some(what) => {
            setup_note!("task differs: {what}");
            TaskHealth::Different
        }
    })
}

/// The first way the registered definition departs from the one `task_xml` describes, or `None`.
fn difference(
    definition: &ITaskDefinition,
    installer: &Path,
    own: &Own,
) -> Result<Option<&'static str>, ComError> {
    // Principal: this account, an ordinary (not elevated) interactive token.
    let principal = com!(definition.Principal())?;
    let mut user = BSTR::new();
    let mut group = BSTR::new();
    let mut logon = TASK_LOGON_TYPE::default();
    let mut level = TASK_RUNLEVEL_TYPE::default();
    com!(principal.UserId(&mut user))?;
    com!(principal.GroupId(&mut group))?;
    com!(principal.LogonType(&mut logon))?;
    com!(principal.RunLevel(&mut level))?;
    if !group.is_empty() || logon != TASK_LOGON_INTERACTIVE_TOKEN || level != TASK_RUNLEVEL_LUA {
        return Ok(Some("principal"));
    }
    if !own.names(&bounded_bstr(&user, MAX_ACCOUNT_CHARS)?) {
        return Ok(Some("principal user"));
    }

    // Action: exactly one Exec, the installed installer with the supervisor flag.
    let actions = com!(definition.Actions())?;
    let mut count = 0;
    com!(actions.Count(&mut count))?;
    if count != 1 {
        return Ok(Some("action count"));
    }
    let action = com!(actions.get_Item(1))?;
    let mut kind = TASK_ACTION_TYPE::default();
    com!(action.Type(&mut kind))?;
    if kind != TASK_ACTION_EXEC {
        return Ok(Some("action type"));
    }
    let exec: IExecAction = action.cast()?;
    let mut program = BSTR::new();
    let mut arguments = BSTR::new();
    com!(exec.Path(&mut program))?;
    com!(exec.Arguments(&mut arguments))?;
    let program = bounded_bstr(&program, MAX_PATH_CHARS)?;
    if !same_path(Path::new(unquoted(&program)), installer) {
        return Ok(Some("action program"));
    }
    let arguments = bounded_bstr(&arguments, MAX_ARGUMENT_CHARS)?;
    let arguments = arguments.trim();
    if arguments != SUPERVISOR_FLAG && arguments != SUPERVISOR_ALIAS {
        return Ok(Some("action arguments"));
    }

    // Trigger: at least one enabled LOGON trigger for this account.
    let triggers = com!(definition.Triggers())?;
    let mut count = 0;
    com!(triggers.Count(&mut count))?;
    let mut ours = false;
    for index in 1..=count.min(MAX_ITEMS) {
        let trigger = com!(triggers.get_Item(index))?;
        let mut kind = TASK_TRIGGER_TYPE2::default();
        com!(trigger.Type(&mut kind))?;
        if kind != TASK_TRIGGER_LOGON {
            continue;
        }
        let mut enabled = VARIANT_BOOL(0);
        com!(trigger.Enabled(&mut enabled))?;
        if enabled.0 == 0 {
            continue;
        }
        let trigger: ILogonTrigger = trigger.cast()?;
        let mut trigger_user = BSTR::new();
        com!(trigger.UserId(&mut trigger_user))?;
        if own.names(&bounded_bstr(&trigger_user, MAX_ACCOUNT_CHARS)?) {
            ours = true;
            break;
        }
    }
    if !ours {
        return Ok(Some("logon trigger"));
    }

    // Settings that decide whether the supervisor starts, stays up and runs at normal priority.
    let settings = com!(definition.Settings())?;
    let mut policy = TASK_INSTANCES_POLICY::default();
    let mut on_demand = VARIANT_BOOL(0);
    let mut stop_on_battery = VARIANT_BOOL(0);
    let mut no_start_on_battery = VARIANT_BOOL(0);
    let mut hard_terminate = VARIANT_BOOL(0);
    let mut only_if_idle = VARIANT_BOOL(0);
    let mut only_if_network = VARIANT_BOOL(0);
    let mut limit = BSTR::new();
    let mut priority = 0;
    com!(settings.MultipleInstances(&mut policy))?;
    com!(settings.AllowDemandStart(&mut on_demand))?;
    com!(settings.StopIfGoingOnBatteries(&mut stop_on_battery))?;
    com!(settings.DisallowStartIfOnBatteries(&mut no_start_on_battery))?;
    com!(settings.AllowHardTerminate(&mut hard_terminate))?;
    com!(settings.RunOnlyIfIdle(&mut only_if_idle))?;
    com!(settings.RunOnlyIfNetworkAvailable(&mut only_if_network))?;
    com!(settings.ExecutionTimeLimit(&mut limit))?;
    com!(settings.Priority(&mut priority))?;
    let settings_differ = policy != TASK_INSTANCES_IGNORE_NEW
        || on_demand.0 == 0
        || stop_on_battery.0 != 0
        || no_start_on_battery.0 != 0
        || hard_terminate.0 == 0
        || only_if_idle.0 != 0
        || only_if_network.0 != 0
        || priority != 4
        || bounded_bstr(&limit, MAX_LIMIT_CHARS)? != "PT0S";
    if settings_differ {
        return Ok(Some("settings"));
    }
    Ok(None)
}

fn folder_is_empty(folder: &ITaskFolder) -> Result<bool, ComError> {
    let tasks = com!(folder.GetTasks(TASK_ENUM_HIDDEN.0))?;
    let folders = com!(folder.GetFolders(0))?;
    Ok(com!(tasks.Count())? == 0 && com!(folders.Count())? == 0)
}

/// A `BSTR` as text, refusing anything oversize, NUL-bearing or not valid UTF-16.
fn bounded_bstr(value: &BSTR, cap: usize) -> Result<String, ComError> {
    if value.len() > cap {
        return Err(ComError::INVALID);
    }
    let mut output = String::new();
    for character in char::decode_utf16(value.iter().copied()) {
        let character = character.map_err(|_| ComError::INVALID)?;
        if character == '\0' || output.len().saturating_add(character.len_utf8()) > cap {
            return Err(ComError::INVALID);
        }
        output.push(character);
    }
    Ok(output)
}

/// Task Scheduler keeps an action path as written; ours is unquoted, but a quoted one still counts.
fn unquoted(text: &str) -> &str {
    let text = text.trim();
    text.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(text)
}

// ---------------------------------------------------------------------------------------------
// XML text
// ---------------------------------------------------------------------------------------------

/// `path` as plain text: without the `\\?\` (or `\\?\UNC\`) prefix and without a trailing separator.
fn plain(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.into_owned()
    };
    match text.trim_end_matches('\\') {
        "" => text,
        trimmed => trimmed.to_string(),
    }
}

/// Escapes `& < > " '` so a value can sit inside an element.
fn escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            other => output.push(other),
        }
    }
    output
}

// ---------------------------------------------------------------------------------------------
// COM plumbing
// ---------------------------------------------------------------------------------------------

/// A failed COM call, by `HRESULT` bits only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ComError(u32);

impl ComError {
    /// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`, what Task Scheduler answers for a missing task or folder.
    const NOT_FOUND: u32 = 0x8007_0002;
    /// `HRESULT_FROM_WIN32(ERROR_PATH_NOT_FOUND)`, accepted as the same answer.
    const PATH_NOT_FOUND: u32 = 0x8007_0003;
    /// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`
    const ALREADY_EXISTS: u32 = 0x8007_00B7;
    /// `SCHED_E_TASK_NOT_RUNNING`
    const NOT_RUNNING: u32 = 0x8004_1315;
    /// `HRESULT_FROM_WIN32(ERROR_INVALID_DATA)`, for a value that failed its bounds.
    const INVALID: Self = Self(0x8007_000D);
    /// `E_FAIL`, for a worker thread that could not start or panicked.
    const FAILED: Self = Self(0x8000_4005);

    fn is_missing(self) -> bool {
        matches!(self.0, Self::NOT_FOUND | Self::PATH_NOT_FOUND)
    }
}

impl From<windows::core::Error> for ComError {
    fn from(error: windows::core::Error) -> Self {
        Self(error.code().0 as u32)
    }
}

/// Converts a COM result, logging the `HRESULT` and call site of anything but "not found".
#[track_caller]
fn logged<T>(result: windows::core::Result<T>) -> Result<T, ComError> {
    let at = std::panic::Location::caller();
    result.map_err(|error| {
        let error = ComError::from(error);
        if !error.is_missing() {
            setup_note!(
                "task: com error 0x{:08X} at {}:{}",
                error.0,
                at.file(),
                at.line()
            );
        }
        error
    })
}

/// Logs a failed operation's `HRESULT` and returns the plain-text error for the person using Setup.
fn failed(what: &str, error: ComError, text: &str) -> SetupError {
    setup_note!("task {what} failed: com error 0x{:08X}", error.0);
    SetupError::after(text)
}

/// Whether `CoInitializeSecurity` left the process with COM security in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SecurityOutcome {
    /// Process COM security is in place, either set by this call or already set in this process.
    Ready,
    /// Not established by this call. A later apartment asks again rather than inheriting failure.
    Retry,
}

/// Classifies the `HRESULT` of `CoInitializeSecurity`.
///
/// `RPC_E_TOO_LATE` means this process's COM security was already initialised before this
/// installer's call. In the GUI process, the windowing and rendering stack has already used COM,
/// so that is expected. The Task Scheduler object is created `CLSCTX_INPROC_SERVER`, so it needs
/// no blanket of its own. Every other failure is `Retry`, so a transient error cannot poison the
/// whole GUI session.
fn security_outcome(code: windows::core::HRESULT) -> SecurityOutcome {
    if code.is_ok() || code == RPC_E_TOO_LATE {
        SecurityOutcome::Ready
    } else {
        SecurityOutcome::Retry
    }
}

/// Set only after a `Ready` outcome. Failures are never cached, so a later apartment retries.
static SECURITY_READY: AtomicBool = AtomicBool::new(false);

/// One balanced `CoInitializeEx(COINIT_MULTITHREADED)` scope on the thread that made it.
#[derive(Debug)]
struct Apartment(PhantomData<Rc<()>>);

impl Apartment {
    fn enter() -> Result<Self, ComError> {
        // SAFETY: this newly created worker thread initialises only its own MTA apartment.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(ComError::from)?;
        // Built right after the successful CoInitializeEx: any early return below drops it, so
        // CoUninitialize still balances on this thread.
        let apartment = Self(PhantomData);
        if !SECURITY_READY.load(Ordering::Acquire) {
            // SAFETY: standard process COM security, no custom descriptor or borrowed list.
            // RPC_E_TOO_LATE is accepted by `security_outcome`; any other failure is retried.
            let result = unsafe {
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
            };
            let outcome = match result {
                Ok(()) => SecurityOutcome::Ready,
                Err(error) => security_outcome(error.code()),
            };
            match outcome {
                SecurityOutcome::Ready => SECURITY_READY.store(true, Ordering::Release),
                SecurityOutcome::Retry => {
                    setup_note!("task: COM security could not be set up");
                    return Err(ComError::FAILED);
                }
            }
        }
        Ok(apartment)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful initialisation made by `enter` on this exact thread.
        unsafe {
            CoUninitialize();
        }
    }
}

/// A connection to the local Task Scheduler, alive only inside `on_com_thread`.
#[derive(Debug)]
struct Scheduler {
    service: ITaskService,
    root: ITaskFolder,
    // Declared LAST: COM references are released before the apartment is uninitialised.
    _apartment: Apartment,
}

impl Scheduler {
    fn connect() -> Result<Self, ComError> {
        let apartment = Apartment::enter()?;
        let service: ITaskService =
            com!(CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER))?;
        let empty = VARIANT::default();
        com!(service.Connect(&empty, &empty, &empty, &empty))?;
        let root = com!(service.GetFolder(&BSTR::from("\\")))?;
        Ok(Self {
            service,
            root,
            _apartment: apartment,
        })
    }

    /// The folder at `path`, or `None` when it does not exist.
    fn folder(&self, path: &str) -> Result<Option<ITaskFolder>, ComError> {
        match com!(self.service.GetFolder(&BSTR::from(path))) {
            Ok(folder) => Ok(Some(folder)),
            Err(error) if error.is_missing() => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// `\Crosspane`, created when it is not there yet.
    fn ensure_folder(&self) -> Result<ITaskFolder, ComError> {
        if let Some(folder) = self.folder(FOLDER_PATH)? {
            return Ok(folder);
        }
        match com!(
            self.root
                .CreateFolder(&BSTR::from(FOLDER_NAME), &VARIANT::default())
        ) {
            Ok(folder) => Ok(folder),
            // Someone created it between the lookup and here.
            Err(error) if error.0 == ComError::ALREADY_EXISTS => {
                self.folder(FOLDER_PATH)?.ok_or(error)
            }
            Err(error) => Err(error),
        }
    }

    /// `\Crosspane\Agent`, or `None` when it does not exist.
    fn task(&self) -> Result<Option<IRegisteredTask>, ComError> {
        match com!(self.root.GetTask(&BSTR::from(TASK_PATH))) {
            Ok(task) => Ok(Some(task)),
            Err(error) if error.is_missing() => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// Runs `work` on a fresh thread that owns a balanced MTA scope and a Task Scheduler connection.
/// Nothing COM-related crosses back: `T` is plain data, and every interface is released before the
/// scope ends. A panic or a thread that cannot start is an `E_FAIL`, never a hang.
fn on_com_thread<T, F>(work: F) -> Result<T, ComError>
where
    T: Send,
    F: FnOnce(&Scheduler) -> Result<T, ComError> + Send,
{
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("task-scheduler".into())
            .spawn_scoped(scope, move || {
                let scheduler = Scheduler::connect()?;
                work(&scheduler)
            });
        match worker {
            Ok(handle) => handle.join().unwrap_or_else(|_| {
                setup_note!("task: the Task Scheduler worker panicked");
                Err(ComError::FAILED)
            }),
            Err(_) => {
                setup_note!("task: the Task Scheduler worker could not start");
                Err(ComError::FAILED)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use windows::Win32::Foundation::{E_ACCESSDENIED, E_OUTOFMEMORY, S_FALSE, S_OK};

    const SID: &str = "S-1-5-21-3293377492-820895984-3446235560-1001";

    /// Only `install` matters to `task_xml`; the other folders sit beside it.
    fn paths_with_install(install: &str) -> Paths {
        let base = PathBuf::from(r"C:\Users\someone\AppData");
        Paths {
            install: PathBuf::from(install),
            programs: base.join(r"Local\Programs"),
            state: base.join(r"Local\Crosspane"),
            installer_state: base.join(r"Local\Crosspane\Installer"),
            backups: base.join(r"Local\Crosspane\backups"),
            runtime: base.join(r"Local\Crosspane\runtime"),
            config: base.join(r"Roaming\Crosspane"),
            start_menu: base.join(r"Roaming\Start Menu\Programs\Crosspane"),
            temp: base.join(r"Local\Temp\Crosspane"),
        }
    }

    fn plain_paths() -> Paths {
        paths_with_install(r"C:\Users\someone\AppData\Local\Programs\Crosspane")
    }

    #[test]
    fn xml_escapes_every_special_character_in_the_install_path() {
        let paths = paths_with_install(r#"C:\a&b<c>d"e'f\Crosspane"#);
        let xml = task_xml(&paths, SID);
        assert!(
            xml.contains(r"C:\a&amp;b&lt;c&gt;d&quot;e&apos;f\Crosspane\crosspane-installer.exe")
        );
        assert!(xml.contains(
            r"<WorkingDirectory>C:\a&amp;b&lt;c&gt;d&quot;e&apos;f\Crosspane</WorkingDirectory>"
        ));
        for raw in ["a&b", "b<c", "c>d", "d\"e", "e'f"] {
            assert!(!xml.contains(raw), "unescaped {raw} in {xml}");
        }
    }

    #[test]
    fn xml_escapes_the_user_id() {
        let xml = task_xml(&plain_paths(), "S-1-5-<&>");
        assert_eq!(
            xml.matches("<UserId>S-1-5-&lt;&amp;&gt;</UserId>").count(),
            2
        );
    }

    #[test]
    fn xml_carries_the_frozen_settings() {
        let xml = task_xml(&plain_paths(), SID);
        for needle in [
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<Priority>4</Priority>",
            "<AllowHardTerminate>true</AllowHardTerminate>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<RunLevel>LeastPrivilege</RunLevel>",
            "<LogonType>InteractiveToken</LogonType>",
            "<Arguments>--windows-supervisor</Arguments>",
            "<URI>\\Crosspane\\Agent</URI>",
            "<LogonTrigger><Enabled>true</Enabled>",
            "<Actions Context=\"Author\">",
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?>",
        ] {
            assert!(xml.contains(needle), "missing {needle} in {xml}");
        }
        assert_eq!(xml.matches(SID).count(), 2);
    }

    #[test]
    fn xml_runs_the_installed_installer_from_the_install_folder() {
        let xml = task_xml(&plain_paths(), SID);
        assert!(xml.contains(
            r"<Command>C:\Users\someone\AppData\Local\Programs\Crosspane\crosspane-installer.exe</Command>"
        ));
        assert!(xml.contains(
            r"<WorkingDirectory>C:\Users\someone\AppData\Local\Programs\Crosspane</WorkingDirectory>"
        ));
    }

    #[test]
    fn xml_drops_the_verbatim_prefix() {
        let verbatim = paths_with_install(r"\\?\C:\Users\someone\AppData\Local\Programs\Crosspane");
        assert_eq!(task_xml(&verbatim, SID), task_xml(&plain_paths(), SID));
        let share = paths_with_install(r"\\?\UNC\server\share\Crosspane");
        assert!(
            task_xml(&share, SID)
                .contains(r"<WorkingDirectory>\\server\share\Crosspane</WorkingDirectory>")
        );
    }

    #[test]
    fn plain_text_helpers() {
        assert_eq!(plain(Path::new(r"C:\x\y\")), r"C:\x\y");
        assert_eq!(plain(Path::new(r"\\?\C:\x")), r"C:\x");
        assert_eq!(escape("a&<>\"'b"), "a&amp;&lt;&gt;&quot;&apos;b");
        assert_eq!(unquoted(r#" "C:\x\y.exe" "#), r"C:\x\y.exe");
        assert_eq!(unquoted(r"C:\x\y.exe"), r"C:\x\y.exe");
    }

    #[test]
    fn the_account_matches_by_sid_domain_name_or_bare_name() {
        let account = Some(r"FROSTDEV\magnu");
        for value in [
            SID,
            "s-1-5-21-3293377492-820895984-3446235560-1001",
            r"FROSTDEV\magnu",
            r"frostdev\MAGNU",
            "magnu",
            "MAGNU",
        ] {
            assert!(names_account(value, SID, account), "{value} should match");
        }
    }

    #[test]
    fn the_account_rejects_everything_else() {
        let account = Some(r"FROSTDEV\magnu");
        for value in [
            "",
            "someone",
            "magnu2",
            "magn",
            r"OTHERDOMAIN\magnu",
            r"FROSTDEV\someone",
            r"FROSTDEV\magnu\extra",
            "S-1-5-21-3293377492-820895984-3446235560-1002",
            "S-1-5-18",
        ] {
            assert!(
                !names_account(value, SID, account),
                "{value} should not match"
            );
        }
    }

    #[test]
    fn without_an_account_name_only_the_sid_matches() {
        assert!(names_account(SID, SID, None));
        assert!(!names_account("magnu", SID, None));
        assert!(!names_account(r"FROSTDEV\magnu", SID, None));
        assert!(!names_account("", SID, None));
    }

    #[test]
    fn a_bare_account_name_does_not_match_an_empty_value() {
        assert!(!names_account("", SID, Some(r"DOMAIN\")));
        assert!(!names_account("", SID, Some("name")));
    }

    #[test]
    fn missing_codes_are_recognised() {
        assert!(ComError(0x8007_0002).is_missing());
        assert!(ComError(0x8007_0003).is_missing());
        assert!(!ComError(0x8007_0005).is_missing());
        assert!(!ComError(ComError::NOT_RUNNING).is_missing());
    }

    #[test]
    fn com_security_too_late_is_ready() {
        assert_eq!(security_outcome(RPC_E_TOO_LATE), SecurityOutcome::Ready);
    }

    #[test]
    fn com_security_success_is_ready() {
        assert_eq!(security_outcome(S_OK), SecurityOutcome::Ready);
        assert_eq!(security_outcome(S_FALSE), SecurityOutcome::Ready);
    }

    #[test]
    fn com_security_other_failure_is_retried() {
        assert_eq!(security_outcome(E_ACCESSDENIED), SecurityOutcome::Retry);
        assert_eq!(security_outcome(E_OUTOFMEMORY), SecurityOutcome::Retry);
    }
}
