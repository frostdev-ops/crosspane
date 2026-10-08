//! Native helper dispatcher (WP-W4.1c, T19 and T21). Admits the frozen command line and refuses
//! anything else before any effect. A mutating verb then runs observe, plan, apply, re-observe and
//! verify. Foreign objects are compared and never stored or printed.

pub(crate) mod driver_apply;
pub(crate) mod driver_observe;
pub(crate) mod firewall;

use std::ffi::OsString;
use std::io::Write;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use crosspane_installer_core::elevated::driver::{
    DRIVER_INF, InstallPlan, InstallStep, Ownership, RemovalPlan, RemovalStep, device_ownership,
    driver_state, package_ownership, plan_install, plan_removal,
};
use crosspane_installer_core::elevated::firewall::{
    ComRule, FirewallPlan, desired_rule, firewall_state, plan_add, plan_remove,
};
use crosspane_installer_core::elevated::status::{
    DeviceStatus, DriverStatus, FirewallStatus, STATUS_SCHEMA, StatusReport,
};
use crosspane_installer_core::elevated::{
    AgentProgram, DRIVER_DIRECTORY, DriverState, FirewallState, MAX_PROGRAM, Outcome, RuleScope,
    Verb, parse_arguments,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, FALSE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, FileAttributeTagInfo,
    GetFileInformationByHandleEx, GetFileType, GetFinalPathNameByHandleW, OPEN_EXISTING,
    VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use self::driver_apply::{
    bind, create_device, delete_package, noninteractive, remove_device, stage,
};
use self::firewall::Firewall;

/// How long a driver mutation may take to show its result before the helper gives up.
const SETTLE: Duration = Duration::from_secs(10);
/// The interval between driver observations while the helper waits for `SETTLE`.
const POLL: Duration = Duration::from_millis(250);

/// Helper-local native failure; value-free (never carries foreign names).
#[derive(Debug)]
pub(crate) enum HelperError {
    /// A COM call failed with this HRESULT.
    Com(i32),
    /// A Win32/SetupAPI/CfgMgr call failed: (call name, error or CONFIGRET code).
    Win32(&'static str, u32),
    /// More objects than the fixed bound.
    Oversize,
    /// A required fact could not be read.
    Unavailable(&'static str),
    /// Refused before any effect.
    Refused(&'static str),
}

/// Result alias for the helper's native modules.
pub(crate) type HelperResult<T> = Result<T, HelperError>;

/// Runs one verb and returns the process exit code. `status` prints its report and exits 0.
pub(crate) fn entry(arguments: &[OsString]) -> i32 {
    run(arguments)
}

/// The dispatcher: DLL hardening first, then parse, the elevation check and the verb. It returns
/// the process exit code: one part's outcome, or the pair code of a combined verb. `setup` and
/// `teardown` always run both parts, in run order, and each part keeps its own rollback.
fn run(arguments: &[OsString]) -> i32 {
    if let Err(error) = harden_dll_search() {
        return failure(error).exit_code();
    }
    let Some(texts) = arguments
        .iter()
        .map(|argument| argument.to_str())
        .collect::<Option<Vec<&str>>>()
    else {
        note("arguments are not UTF-8");
        return Outcome::Refused.exit_code();
    };
    let Ok(verb) = parse_arguments(&texts) else {
        note("unsupported arguments");
        return Outcome::Refused.exit_code();
    };
    if verb.mutates() && !elevated() {
        note("this verb needs an elevated token");
        return Outcome::NotElevated.exit_code();
    }
    let result = match verb {
        Verb::Status(scope) => Ok(print_status(&observe_status(scope.as_ref()))),
        Verb::InstallDriver => install_driver(),
        Verb::RemoveDriver => remove_driver(),
        Verb::AddFirewall(scope) => add_firewall(&scope),
        Verb::RemoveFirewall(scope) => remove_firewall(&scope),
        Verb::Setup(scope) => {
            return Outcome::pair_exit_code(
                add_firewall(&scope).unwrap_or_else(failure),
                install_driver().unwrap_or_else(failure),
            );
        }
        Verb::Teardown(scope) => {
            return Outcome::pair_exit_code(
                remove_driver().unwrap_or_else(failure),
                remove_firewall(&scope).unwrap_or_else(failure),
            );
        }
    };
    result.unwrap_or_else(failure).exit_code()
}

/// T21, the first call of the process. The default DLL search is System32 plus explicitly added
/// directories, so a DLL planted beside the helper is never loaded.
fn harden_dll_search() -> HelperResult<()> {
    // SAFETY: changes only this process's default DLL search directories, with a constant flag.
    if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } == FALSE {
        return Err(HelperError::Win32("SetDefaultDllDirectories", last_error()));
    }
    Ok(())
}

/// True when this process holds an elevated token. A token that cannot be read is not elevated.
fn elevated() -> bool {
    let mut raw: HANDLE = null_mut();
    // SAFETY: the current-process pseudo handle needs no closing. `raw` is a live output that
    // receives a token handle only on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == FALSE {
        return false;
    }
    let token = Handle(raw);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: `token` is open for TOKEN_QUERY; `elevation` is a live structure whose exact size is
    // passed; `returned` is a live output.
    let read = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    read != FALSE && elevation.TokenIsElevated != 0
}

/// `status`: the report of the driver and firewall state. It never fails. A part that cannot be
/// read is `Unavailable`, and a firewall rule that was not asked for is `NotRequested`.
fn observe_status(scope: Option<&RuleScope>) -> StatusReport {
    let report = StatusReport {
        schema: STATUS_SCHEMA,
        elevated: elevated(),
        driver: observe_driver(),
        firewall: observe_firewall(scope),
    };
    if report.validate().is_ok() {
        report
    } else {
        StatusReport {
            driver: unavailable_driver(),
            ..report
        }
    }
}

/// The driver inventory. Only Crosspane's own packages and devices are listed by name.
fn observe_driver() -> DriverStatus {
    let (packages, devices) = match (driver_observe::packages(), driver_observe::devices()) {
        (Ok(packages), Ok(devices)) => (packages, devices),
        (Err(error), _) | (_, Err(error)) => {
            // Value-free: HelperError carries only call names and numeric codes.
            note(&format!("driver observation unavailable: {error:?}"));
            return unavailable_driver();
        }
    };
    DriverStatus {
        state: driver_state(&packages, &devices),
        packages: packages
            .iter()
            .filter(|package| package_ownership(package) == Ownership::Ours)
            .map(|package| package.published.clone())
            .collect(),
        devices: devices
            .iter()
            .filter(|device| device_ownership(device) == Ownership::Ours)
            .map(|device| DeviceStatus {
                instance_id: device.instance_id.clone(),
                driver_inf: device.driver_inf.clone(),
                problem: device.problem,
                present: device.present,
            })
            .collect(),
    }
}

/// The firewall state for `requested`, the family count and whether local rules apply.
fn observe_firewall(requested: Option<&RuleScope>) -> FirewallStatus {
    let (family, local_rules_apply): (HelperResult<Vec<ComRule>>, Option<bool>) =
        match Firewall::open() {
            Ok(firewall) => (firewall.family(), firewall.local_rules_apply().ok()),
            Err(error) => (Err(error), None),
        };
    let state = match (requested, &family) {
        (None, _) => FirewallState::NotRequested,
        (Some(scope), Ok(rules)) => firewall_state(rules, Some(scope)),
        (Some(_), Err(_)) => FirewallState::Unavailable,
    };
    let family_count = family.as_ref().map_or(0, Vec::len);
    FirewallStatus {
        state,
        family_count: u32::try_from(family_count).unwrap_or(u32::MAX),
        local_rules_apply,
    }
}

fn unavailable_driver() -> DriverStatus {
    DriverStatus {
        state: DriverState::Unavailable,
        packages: Vec::new(),
        devices: Vec::new(),
    }
}

/// Prints the report as one JSON line on stdout. A failed write is ignored: the launcher then sees
/// no report and does not verify.
fn print_status(report: &StatusReport) -> Outcome {
    match serde_json::to_string(report) {
        Ok(text) => {
            let _ = writeln!(std::io::stdout(), "{text}");
            Outcome::Done
        }
        Err(_) => Outcome::Failed,
    }
}

/// `install-driver`: one Crosspane package and one adapter node, from the package next to the helper.
fn install_driver() -> HelperResult<Outcome> {
    noninteractive()?;
    let directory = driver_directory()?;
    let source = driver_observe::source_package(&directory)?;
    let packages = driver_observe::packages()?;
    let devices = driver_observe::devices()?;
    let steps = match plan_install(&source, &packages, &devices) {
        InstallPlan::AlreadyInstalled => return Ok(Outcome::AlreadyDone),
        InstallPlan::Mismatch(reason) => {
            note(reason);
            return Ok(Outcome::Mismatch);
        }
        InstallPlan::Apply(steps) => steps,
    };
    let windir = driver_observe::inf_directory()?;
    let source_inf = directory.join(DRIVER_INF);
    let published = packages
        .iter()
        .find(|package| package_ownership(package) == Ownership::Ours)
        .map(|package| package.published.clone());
    let mut journal = Journal::default();
    if let Err(error) = apply_install(&steps, &source_inf, &windir, published, &mut journal) {
        rollback(&journal, &windir);
        return Err(error);
    }
    if settle(|state| state == DriverState::Installed) {
        Ok(Outcome::Done)
    } else if journal.reboot {
        Ok(Outcome::RebootRequired)
    } else {
        note("the driver did not reach Installed; the evidence is kept");
        Ok(Outcome::Failed)
    }
}

/// Runs the planned install steps in order. `published` is the name of a package the store already
/// holds; a `Stage` step sets it otherwise.
fn apply_install(
    steps: &[InstallStep],
    source_inf: &Path,
    windir: &Path,
    mut published: Option<String>,
    journal: &mut Journal,
) -> HelperResult<()> {
    for step in steps {
        match step {
            InstallStep::Stage => {
                let staged = stage(source_inf)?;
                if staged.newly {
                    journal.staged = Some(staged.published.clone());
                }
                published = Some(staged.published);
            }
            InstallStep::CreateDevice => {
                let target = windir.join(
                    published
                        .as_deref()
                        .ok_or(HelperError::Unavailable("driver package name"))?,
                );
                journal.device = Some(create_device(source_inf)?);
                // Creating a node does not bind it, so the package is always bound right after.
                journal.reboot |= bind(&target)?;
            }
            InstallStep::Bind => {
                let target = windir.join(
                    published
                        .as_deref()
                        .ok_or(HelperError::Unavailable("driver package name"))?,
                );
                journal.reboot |= bind(&target)?;
            }
        }
    }
    Ok(())
}

/// `remove-driver`: the adapter nodes first, then the packages. Nothing is created, so a failed step
/// undoes nothing and leaves the evidence in place.
fn remove_driver() -> HelperResult<Outcome> {
    noninteractive()?;
    let packages = driver_observe::packages()?;
    let devices = driver_observe::devices()?;
    let steps = match plan_removal(&packages, &devices) {
        RemovalPlan::AlreadyAbsent => return Ok(Outcome::AlreadyDone),
        RemovalPlan::Mismatch(reason) => {
            note(reason);
            return Ok(Outcome::Mismatch);
        }
        RemovalPlan::Apply(steps) => steps,
    };
    let windir = driver_observe::inf_directory()?;
    let mut reboot = false;
    for step in &steps {
        reboot |= match step {
            RemovalStep::RemoveDevice(id) => remove_device(id)?,
            RemovalStep::DeletePackage(name) => delete_package(&windir.join(name))?,
        };
    }
    if settle(|state| state == DriverState::Absent) {
        Ok(Outcome::Done)
    } else if reboot {
        Ok(Outcome::RebootRequired)
    } else {
        note("the driver did not reach Absent; the remaining evidence is kept");
        Ok(Outcome::Failed)
    }
}

/// `add-firewall`: one rule for one agent program. The program must be an existing canonical
/// regular file, checked before the firewall is touched.
fn add_firewall(scope: &RuleScope) -> HelperResult<Outcome> {
    if !is_canonical_file(&scope.program) {
        note("the agent program is not an existing canonical regular file");
        return Ok(Outcome::Refused);
    }
    let firewall = Firewall::open()?;
    let family = firewall.family()?;
    match plan_add(&family, scope) {
        FirewallPlan::AlreadyPresent => return Ok(Outcome::AlreadyDone),
        FirewallPlan::Mismatch(reason) => {
            note(reason);
            return Ok(Outcome::Mismatch);
        }
        FirewallPlan::Add => {}
        FirewallPlan::Remove | FirewallPlan::AlreadyAbsent => return Ok(Outcome::Failed),
    }
    firewall.add(&desired_rule(scope))?;
    if firewall_reads(&firewall, scope, FirewallState::Present) {
        return Ok(Outcome::Done);
    }
    // Verification failed after the add: remove exactly the rule this run added.
    if firewall.remove(&scope.id.rule_name()).is_err() {
        note("could not remove the firewall rule this run added");
    }
    Ok(Outcome::Failed)
}

/// `remove-firewall`: removes the one rule for this agent program, if it matches the specification.
fn remove_firewall(scope: &RuleScope) -> HelperResult<Outcome> {
    let firewall = Firewall::open()?;
    let family = firewall.family()?;
    match plan_remove(&family, scope) {
        FirewallPlan::AlreadyAbsent => return Ok(Outcome::AlreadyDone),
        FirewallPlan::Mismatch(reason) => {
            note(reason);
            return Ok(Outcome::Mismatch);
        }
        FirewallPlan::Remove => {}
        FirewallPlan::Add | FirewallPlan::AlreadyPresent => return Ok(Outcome::Failed),
    }
    firewall.remove(&scope.id.rule_name())?;
    if firewall_reads(&firewall, scope, FirewallState::Missing) {
        Ok(Outcome::Done)
    } else {
        note("the firewall rule is still present after removal");
        Ok(Outcome::Failed)
    }
}

/// True when the rule family, read again now, shows `wanted` for `scope`. An unreadable family is
/// not verified.
fn firewall_reads(firewall: &Firewall, scope: &RuleScope, wanted: FirewallState) -> bool {
    firewall
        .family()
        .is_ok_and(|family| firewall_state(&family, Some(scope)) == wanted)
}

/// The undo list of one install run: the objects it created, and whether a reboot is pending. A
/// failed run undoes exactly these.
#[derive(Debug, Default)]
struct Journal {
    /// Published name of the package this run newly staged.
    staged: Option<String>,
    /// Instance ID of the adapter node this run created.
    device: Option<String>,
    /// A native step reported that a reboot is needed.
    reboot: bool,
}

/// Undoes what the run created: the device node first, then the package it newly staged. Both undos
/// are attempted. A failed undo is noted and otherwise ignored, because the run has already failed.
fn rollback(journal: &Journal, windir: &Path) {
    let device_undone = journal
        .device
        .as_deref()
        .is_none_or(|id| remove_device(id).is_ok());
    let package_undone = journal
        .staged
        .as_deref()
        .is_none_or(|name| delete_package(&windir.join(name)).is_ok());
    if !(device_undone && package_undone) {
        note("part of what this run created could not be removed; the rest is kept");
    }
}

/// `<helper directory>\driver`, where the one Crosspane package sits next to the helper.
fn driver_directory() -> HelperResult<PathBuf> {
    let image = std::env::current_exe().map_err(|_| HelperError::Unavailable("helper location"))?;
    let directory = image
        .parent()
        .ok_or(HelperError::Unavailable("helper location"))?;
    Ok(directory.join(DRIVER_DIRECTORY))
}

/// Polls the driver state until `reached` holds or `SETTLE` has passed. An unreadable state counts
/// as not yet reached. Returns whether the state was reached.
fn settle(reached: fn(DriverState) -> bool) -> bool {
    let deadline = Instant::now() + SETTLE;
    loop {
        if driver_now().is_some_and(reached) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// The driver state now, or `None` when either inventory cannot be read.
fn driver_now() -> Option<DriverState> {
    let packages = driver_observe::packages().ok()?;
    let devices = driver_observe::devices().ok()?;
    Some(driver_state(&packages, &devices))
}

/// True when `program` names an existing regular file whose canonical path is `program` itself. A
/// directory, a reparse point (link or junction), a non-disk file, or a path that resolves elsewhere
/// is refused.
fn is_canonical_file(program: &AgentProgram) -> bool {
    let name = wide(program.as_str());
    // SAFETY: `name` is NUL-terminated and lives for the call. The open reads attributes only, shares
    // every access, and opens a final reparse point rather than following it.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return false;
    }
    let file = Handle(raw);
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: `file` is live; `tag` is the structure FileAttributeTagInfo fills, and its exact size is
    // passed.
    let tagged = unsafe {
        GetFileInformationByHandleEx(
            file.0,
            FileAttributeTagInfo,
            (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    // SAFETY: `file` is live and is only queried for its type.
    let kind = unsafe { GetFileType(file.0) };
    if tagged == FALSE
        || kind != FILE_TYPE_DISK
        || tag.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
    {
        return false;
    }
    let mut buffer = vec![0u16; MAX_PROGRAM + 8];
    // SAFETY: `file` is live; `buffer` is writable and its length in UTF-16 units is passed; the flags
    // ask for the DOS volume name, normalized.
    let units = unsafe {
        GetFinalPathNameByHandleW(
            file.0,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            VOLUME_NAME_DOS | FILE_NAME_NORMALIZED,
        )
    } as usize;
    if units == 0 || units >= buffer.len() {
        return false;
    }
    String::from_utf16(&buffer[..units]).is_ok_and(|path| program.same_path(&path))
}

/// Maps a native failure to its exit code and notes why. Notes carry only static text and OS call
/// names, never a foreign name.
fn failure(error: HelperError) -> Outcome {
    match error {
        HelperError::Refused(reason) => {
            note(reason);
            Outcome::Refused
        }
        HelperError::Oversize => {
            note("more objects than the helper's fixed bound; nothing was changed");
            Outcome::Mismatch
        }
        HelperError::Unavailable(what) => {
            note(&format!("{what} could not be read"));
            Outcome::Failed
        }
        HelperError::Win32(call, code) => {
            note(&format!("{call} failed with error {code}"));
            Outcome::Failed
        }
        HelperError::Com(code) => {
            note(&format!("COM failed with HRESULT {code:#010x}"));
            Outcome::Failed
        }
    }
}

/// Writes one value-free diagnostic line to stderr. A failed write is ignored: the exit code is the
/// result.
fn note(text: &str) {
    let _ = writeln!(std::io::stderr(), "crosspane-elevated-setup: {text}");
}

/// An open kernel handle, closed on every return path.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful open call and is closed exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}

/// The calling thread's last Win32 error.
fn last_error() -> u32 {
    // SAFETY: reads the calling thread's last error code; nothing is changed.
    unsafe { GetLastError() }
}

/// NUL-terminated UTF-16 for a Win32 string argument.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
