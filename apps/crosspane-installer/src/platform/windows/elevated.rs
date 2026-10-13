//! Elevated setup orchestration (L8): firewall rule + display driver through the helper.
//!
//! Nothing here elevates by itself. `observe` runs the helper's read-only `status` unelevated.
//! `run` asks the shell for one `runas` run through `ElevatedHelper::run`, after the person was
//! shown exactly the lines of the plan, and never kills the elevated child. A declined prompt is
//! reported as `Declined`; the install around it still completes.

use super::{
    elevated_kit::{self, KitHold},
    elevated_launch::{ELEVATED_WAIT, ElevatedHelper, LaunchOutcome, STATUS_WAIT},
    engine::Progress,
    error::{NativeError, SetupError, SetupResult},
    paths::{Paths, PayloadFile},
    setup_log::setup_note,
};
use aws_lc_rs::digest::{SHA256, digest};
use crosspane_installer_core::elevated::{
    AgentProgram, DriverState, ElevatedError, FirewallState, HELPER_IMAGE, InstallId,
    MAX_INSTALL_ID, RULE_PREFIX, RuleScope, Verb,
    kit::{KitFile, KitManifest, KitPin},
    status::{Consent, ElevatedJournal, JournalEntry, StatusReport},
    step::{
        ElevatedPlan, INSTALL_WITHOUT_SETUP, PartResult, StepResult, report_lines, status_summary,
    },
};
use std::{
    fs::{self, Metadata, OpenOptions},
    io::Read,
    marker::PhantomData,
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc::{self, RecvTimeoutError},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        NetworkManagement::WindowsFirewall::{INetFwPolicy2, INetFwRule, NetFwPolicy2},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED,
                CoCreateInstance, CoInitializeEx, CoUninitialize, IDispatch,
            },
            Ole::IEnumVARIANT,
            Variant::VARIANT,
        },
    },
    core::Interface,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
};

/// How often `run` tells the person that Windows is still waiting for approval.
const HEARTBEAT: Duration = Duration::from_secs(15);
/// Slack on top of the helper's own waits before `run` stops waiting for it. The elevated child is
/// never killed; only the wait ends.
const RUN_SLACK: Duration = Duration::from_secs(60);
/// Longest the firewall rule scan may work, and how long `scope` waits for its thread.
const SCAN_BUDGET: Duration = Duration::from_secs(25);
const SCAN_WAIT: Duration = Duration::from_secs(30);
/// The most rules one scan looks at, Crosspane's and other products' alike.
const MAX_SCANNED: usize = 65_536;
/// A Crosspane rule name is `RULE_PREFIX` plus an id of at most `MAX_INSTALL_ID` characters.
const MAX_RULE_NAME_UNITS: usize = RULE_PREFIX.len() + MAX_INSTALL_ID;

const WAITING: &str = "Waiting for administrator approval…";
const NO_KIT: &str = "This installer build doesn't include Crosspane's administrator setup files, so the firewall rule and display driver can't be checked.";
const NOT_INSTALLED_YET: &str = "Crosspane's administrator setup files aren't installed yet.";
const PAYLOAD_DAMAGED: &str = "The administrator setup file beside this installer doesn't match it. Download Crosspane again.";
const HELPER_MISSING: &str = "Crosspane's administrator setup file is missing.";
const STATUS_UNREADABLE: &str =
    "Crosspane couldn't read the state of its firewall rule and display driver.";
const KIT_UNAVAILABLE: &str = "The administrator setup files aren't installed or don't match this installer. Run the Crosspane installer you downloaded to restore Crosspane's files.";
const CONSENT_MISMATCH: &str =
    "The administrator step doesn't match what was shown. Close this and try again.";
const AGENT_MOVED: &str =
    "Crosspane isn't installed where the administrator step expected. Try again.";
const COULD_NOT_START: &str = "Windows administrator setup couldn't start. Try again.";
const STOPPED: &str = "Windows administrator setup stopped unexpectedly. Try again.";
const TIMED_OUT: &str = "Windows administrator setup didn't finish in time. Check the state of Crosspane and try again.";
const BAD_LOCATION: &str = "Crosspane can't set up its firewall rule for this install location.";
const NO_RANDOM: &str =
    "Windows couldn't provide a random number for Crosspane's firewall rule. Try again.";
const NOT_RUN_MESSAGE: &str = "The administrator step didn't run.";
/// The halves of core `INSTALL_WITHOUT_SETUP`, for a step that asked for only one of them.
const WITHOUT_FIREWALL: &str =
    "Until the firewall rule is added, other computers may not reach this PC.";
const WITHOUT_DRIVER: &str = "Without the display driver, Crosspane mirrors windows in place.";
const SET_UP: &str = "Firewall rule and display driver are set up.";

/// What the administrator step did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ElevatedOutcome {
    NotNeeded,
    Configured(String),
    Declined,
    Failed(String),
}

/// Install id: the existing rule (`INetFwPolicy2`, unelevated) named `RULE_PREFIX*` whose
/// application is the agent; else random (`aws_lc_rs::rand`). Program = `paths.file(Agent)`.
///
/// The helper counts a rule with another name but the same program as covering the agent, which
/// would leave the state a permanent mismatch. Reusing the existing rule's id avoids that. A scan
/// that fails falls back to a fresh id; only a scope that can't be built is an error.
pub(crate) fn scope(paths: &Paths) -> SetupResult<RuleScope> {
    let program = agent_program(paths)?;
    let id = match existing_install_id(&program) {
        Ok(Some(id)) => id,
        Ok(None) => fresh_install_id()?,
        Err(note) => {
            setup_note!("firewall rule scan failed ({note}); using a new install id");
            fresh_install_id()?
        }
    };
    Ok(RuleScope { id, program })
}

/// Unelevated `status` via the installed helper, else `payload\crosspane-elevated-setup.exe`
/// (verified source). The helper's files are held unreplaceable while it runs.
pub(crate) fn observe(
    paths: &Paths,
    payload: Option<&Path>,
    scope: &RuleScope,
) -> Result<StatusReport, String> {
    let Some(kit) = elevated_kit::embedded() else {
        return Err(NO_KIT.to_owned());
    };
    let (image, _held) = status_helper(paths, payload, &kit)?;
    let helper = ElevatedHelper::locate(image).map_err(|error| match error {
        NativeError::Missing => HELPER_MISSING.to_owned(),
        _ => STATUS_UNREADABLE.to_owned(),
    })?;
    helper.status(Some(scope)).map_err(|error| {
        setup_note!("elevated status failed: {error}");
        match error {
            NativeError::Missing => HELPER_MISSING.to_owned(),
            _ => STATUS_UNREADABLE.to_owned(),
        }
    })
}

/// The firewall rule is missing or is Crosspane's own rule in a shape setup puts right.
fn firewall_needs_setup(report: &StatusReport) -> bool {
    matches!(
        report.firewall.state,
        FirewallState::Missing | FirewallState::Mismatch
    )
}

/// The display driver is absent or only partly installed. A driver `Mismatch` is not included: the
/// helper's `setup` refuses it (`plan_install` returns `Mismatch` for every shape `driver_state`
/// calls `Mismatch`), so asking would only repeat a refused prompt.
fn driver_needs_setup(report: &StatusReport) -> bool {
    matches!(
        report.driver.state,
        DriverState::Absent | DriverState::PackageOnly | DriverState::DeviceWithoutDriver
    )
}

/// A firewall rule is there for `teardown` to remove, in any form.
fn firewall_to_remove(report: &StatusReport) -> bool {
    matches!(
        report.firewall.state,
        FirewallState::Present | FirewallState::Mismatch
    )
}

/// A display driver is there for `teardown` to remove, in any form.
fn driver_to_remove(report: &StatusReport) -> bool {
    matches!(
        report.driver.state,
        DriverState::PackageOnly
            | DriverState::DeviceWithoutDriver
            | DriverState::Installed
            | DriverState::Mismatch
    )
}

/// firewall ∈ {Missing, Mismatch} || driver ∈ {Absent, PackageOnly, DeviceWithoutDriver}. A state
/// that couldn't be read is not a reason to ask for administrator approval. A driver `Mismatch`
/// isn't either (see `driver_needs_setup`).
pub(crate) fn setup_needed(report: &StatusReport) -> bool {
    firewall_needs_setup(report) || driver_needs_setup(report)
}

/// Whether there is a firewall rule or driver left for `teardown` to remove, in any form. A state
/// that couldn't be read is not a reason to ask for administrator approval.
pub(crate) fn teardown_needed(report: &StatusReport) -> bool {
    firewall_to_remove(report) || driver_to_remove(report)
}

/// The administrator step that sets up only what `report` shows is missing: `AddFirewall` when
/// only the firewall rule is, `InstallDriver` when only the driver is, `Setup` when both are, and
/// `None` when neither is (`setup_needed` false). The person is then asked about, and told about,
/// exactly what will be done.
pub(crate) fn setup_verb(report: &StatusReport, scope: RuleScope) -> Option<Verb> {
    match (firewall_needs_setup(report), driver_needs_setup(report)) {
        (true, true) => Some(Verb::Setup(scope)),
        (true, false) => Some(Verb::AddFirewall(scope)),
        (false, true) => Some(Verb::InstallDriver),
        (false, false) => None,
    }
}

/// The administrator step that removes only what `report` shows is there: `RemoveFirewall`,
/// `RemoveDriver`, `Teardown` for both, and `None` when neither is (`teardown_needed` false).
pub(crate) fn teardown_verb(report: &StatusReport, scope: RuleScope) -> Option<Verb> {
    match (firewall_to_remove(report), driver_to_remove(report)) {
        (true, true) => Some(Verb::Teardown(scope)),
        (true, false) => Some(Verb::RemoveFirewall(scope)),
        (false, true) => Some(Verb::RemoveDriver),
        (false, false) => None,
    }
}

/// Whether running `consented` does any part of `wanted`, so the person who agreed to its lines
/// agreed to something `wanted` still calls for. Parts are compared by kind: the scope is the one
/// the plan was made with.
pub(crate) fn shares_a_part(consented: &Verb, wanted: &Verb) -> bool {
    let wanted = wanted.parts();
    consented
        .parts()
        .iter()
        .any(|part| wanted.iter().any(|other| other.name() == part.name()))
}

/// What it means that the person declined `verb`: the core sentence when both the firewall rule
/// and the driver were asked for, otherwise only the half that was.
pub(crate) fn without_setup_note(verb: &Verb) -> &'static str {
    match verb {
        Verb::AddFirewall(_) => WITHOUT_FIREWALL,
        Verb::InstallDriver => WITHOUT_DRIVER,
        _ => INSTALL_WITHOUT_SETUP,
    }
}

/// The state of the firewall rule and display driver in plain words, for the line under the step:
/// what administrator approval is needed for, or that both are set up. A state that is neither
/// (unreadable, not Crosspane's) is told with the core summary.
pub(crate) fn plain_status(report: &StatusReport) -> String {
    let wants = match (firewall_needs_setup(report), driver_needs_setup(report)) {
        (true, true) => "allow Crosspane through the firewall and add the display driver",
        (true, false) => "allow Crosspane through the firewall",
        (false, true) => "add the display driver",
        (false, false) => {
            let (configured, text) = status_summary(report, false);
            return if configured { SET_UP.to_owned() } else { text };
        }
    };
    format!("Needs administrator approval to {wants}.")
}

/// `ElevatedPlan::new` for any mutating verb: `Setup`, `AddFirewall` and `InstallDriver`, or
/// `Teardown`, `RemoveFirewall` and `RemoveDriver`.
pub(crate) fn plan(verb: Verb) -> SetupResult<ElevatedPlan> {
    ElevatedPlan::new(verb).map_err(|error: ElevatedError| {
        setup_note!("elevated plan refused: {error}");
        SetupError::before(CONSENT_MISMATCH)
    })
}

/// `hold_placed` for the run; `ElevatedHelper::run` on a dedicated STA thread
/// (`CoInitializeEx` APARTMENTTHREADED); log-only `ElevatedJournal`; heartbeat progress every
/// 15 s; text from core `status_summary`.
pub(crate) fn run(
    paths: &Paths,
    kit: &KitManifest,
    plan: &ElevatedPlan,
    progress: &dyn Progress,
) -> ElevatedOutcome {
    let verb = plan.verb().clone();
    let Ok(consent) = plan.consent() else {
        return ElevatedOutcome::Failed(CONSENT_MISMATCH.to_owned());
    };
    if matches!(verb, Verb::Setup(_) | Verb::AddFirewall(_)) && !plans_installed_agent(paths, &verb)
    {
        return ElevatedOutcome::Failed(AGENT_MOVED.to_owned());
    }
    // The hold keeps the four kit files unreplaceable until the helper has run. It moves into the
    // worker, so it lives as long as the helper does even if this wait gives up.
    let (image, hold) = match elevated_kit::hold_placed(&paths.install, kit) {
        Ok(held) => held,
        Err(error) => {
            setup_note!("elevated kit not usable: {error}");
            return ElevatedOutcome::Failed(KIT_UNAVAILABLE.to_owned());
        }
    };
    let helper = match ElevatedHelper::locate(image) {
        Ok(helper) => helper,
        Err(error) => {
            setup_note!("elevated helper not usable: {error}");
            return ElevatedOutcome::Failed(KIT_UNAVAILABLE.to_owned());
        }
    };
    setup_note!("elevated {} starting", verb.name().as_str());

    let (sender, receiver) = mpsc::channel();
    let worker_verb = verb.clone();
    let spawned = std::thread::Builder::new()
        .name("crosspane-elevated-run".into())
        .spawn(move || {
            let result = run_on_sta(&helper, &worker_verb, &consent);
            drop(hold);
            let _ = sender.send(result);
        });
    if let Err(error) = spawned {
        setup_note!(
            "elevated worker did not start: os error {:?}",
            error.raw_os_error()
        );
        return ElevatedOutcome::Failed(COULD_NOT_START.to_owned());
    }

    let give_up = STATUS_WAIT
        .saturating_mul(2)
        .saturating_add(ELEVATED_WAIT)
        .saturating_add(RUN_SLACK);
    let started = Instant::now();
    progress.step(WAITING);
    let result = loop {
        let left = give_up.saturating_sub(started.elapsed());
        match receiver.recv_timeout(HEARTBEAT.min(left)) {
            Ok(result) => break result,
            Err(RecvTimeoutError::Timeout) if started.elapsed() >= give_up => {
                setup_note!("elevated {} wait gave up", verb.name().as_str());
                return ElevatedOutcome::Failed(TIMED_OUT.to_owned());
            }
            Err(RecvTimeoutError::Timeout) => progress.step(WAITING),
            Err(RecvTimeoutError::Disconnected) => {
                setup_note!(
                    "elevated {} worker ended without a result",
                    verb.name().as_str()
                );
                return ElevatedOutcome::Failed(STOPPED.to_owned());
            }
        }
    };
    let outcome = match result {
        Ok(launched) => outcome_of(&verb, launched),
        Err(error) => {
            setup_note!("elevated {} launch failed: {error}", verb.name().as_str());
            ElevatedOutcome::Failed(match error {
                NativeError::Missing => HELPER_MISSING.to_owned(),
                NativeError::Timeout | NativeError::Cancelled => TIMED_OUT.to_owned(),
                _ => COULD_NOT_START.to_owned(),
            })
        }
    };
    setup_note!(
        "elevated {} finished: {}",
        verb.name().as_str(),
        kind(&outcome)
    );
    outcome
}

/// Core `status_summary` text for `report`.
pub(crate) fn summary(report: &StatusReport) -> String {
    status_summary(report, false).1
}

/// A short label for the setup log. It never carries the outcome's text.
fn kind(outcome: &ElevatedOutcome) -> &'static str {
    match outcome {
        ElevatedOutcome::NotNeeded => "not needed",
        ElevatedOutcome::Configured(_) => "configured",
        ElevatedOutcome::Declined => "declined",
        ElevatedOutcome::Failed(_) => "failed",
    }
}

// --- the scope -------------------------------------------------------------------------------

/// The agent's program path as the helper wants it: a plain drive path without `\\?\`.
fn agent_program(paths: &Paths) -> SetupResult<AgentProgram> {
    let agent = paths.file(PayloadFile::Agent);
    let text = agent
        .to_str()
        .ok_or_else(|| SetupError::before(BAD_LOCATION))?;
    let text = text.strip_prefix(r"\\?\").unwrap_or(text);
    AgentProgram::parse(text).map_err(|_| SetupError::before(BAD_LOCATION))
}

/// A fresh random install id.
fn fresh_install_id() -> SetupResult<InstallId> {
    let mut random = [0_u8; 16];
    aws_lc_rs::rand::fill(&mut random).map_err(|_| SetupError::before(NO_RANDOM))?;
    Ok(InstallId::from_random(random))
}

/// The install id in `name` when it is `RULE_PREFIX` (ASCII case-insensitive, as the rule store
/// compares names) followed by a valid id. Anything else is `None`.
fn install_id_from_rule_name(name: &str) -> Option<InstallId> {
    let head = name.get(..RULE_PREFIX.len())?;
    if !head.eq_ignore_ascii_case(RULE_PREFIX) {
        return None;
    }
    InstallId::parse(name.get(RULE_PREFIX.len()..)?).ok()
}

/// True when a rule name, as UTF-16 units, starts with `RULE_PREFIX` (ASCII case-insensitive).
/// Names of other products' rules are compared in this form only and never converted or logged.
fn has_rule_prefix(name: &[u16]) -> bool {
    let prefix = RULE_PREFIX.as_bytes();
    name.len() >= prefix.len()
        && name.iter().zip(prefix).all(|(unit, byte)| {
            u8::try_from(*unit).is_ok_and(|unit| unit.eq_ignore_ascii_case(byte))
        })
}

/// Why a rule scan produced nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanError {
    /// A COM call failed with this HRESULT.
    Com(i32),
    /// More rules than `MAX_SCANNED`.
    TooMany,
    /// `SCAN_BUDGET` passed.
    Deadline,
}

impl ScanError {
    /// The setup log note. It carries an HRESULT at most, never a rule name.
    fn note(self) -> String {
        match self {
            Self::Com(code) => format!("com error 0x{:08X}", code as u32),
            Self::TooMany => "too many firewall rules".to_owned(),
            Self::Deadline => "the scan took too long".to_owned(),
        }
    }
}

fn com(error: windows::core::Error) -> ScanError {
    ScanError::Com(error.code().0)
}

/// The id of the first existing Crosspane rule whose application is `program`. The scan runs on
/// its own short-lived thread so COM never touches the caller's apartment, and the wait for it is
/// bounded. `Err` is a log note.
fn existing_install_id(program: &AgentProgram) -> Result<Option<InstallId>, String> {
    let (sender, receiver) = mpsc::channel();
    let program = program.clone();
    std::thread::Builder::new()
        .name("crosspane-firewall-scan".into())
        .spawn(move || {
            let _ = sender.send(scan_rules(&program, Instant::now() + SCAN_BUDGET));
        })
        .map_err(|_| "the scan thread couldn't start".to_owned())?;
    match receiver.recv_timeout(SCAN_WAIT) {
        Ok(Ok(found)) => Ok(found),
        Ok(Err(error)) => Err(error.note()),
        Err(_) => Err("the scan didn't finish".to_owned()),
    }
}

/// `CoInitializeEx` on this thread, balanced on drop. The guard is not `Send`.
#[derive(Debug)]
struct Apartment(PhantomData<Rc<()>>);

impl Apartment {
    fn multithreaded() -> Result<Self, ScanError> {
        // SAFETY: initializes COM only for the calling thread. `Drop` balances a success, and the
        // guard can't leave this thread.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(com)?;
        Ok(Self(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful `CoInitializeEx` on this same thread.
        unsafe { CoUninitialize() };
    }
}

/// Read-only: enumerates the firewall rules and returns the install id of the first rule named
/// `RULE_PREFIX*` whose application is `program`. A rule whose name carries no valid id is not
/// ours and is skipped.
fn scan_rules(program: &AgentProgram, deadline: Instant) -> Result<Option<InstallId>, ScanError> {
    // Declared FIRST, so it is dropped LAST: the COM references below are released before the
    // apartment is uninitialized.
    let _apartment = Apartment::multithreaded()?;
    // SAFETY: creates the in-process `NetFwPolicy2` object on the apartment initialized above.
    // Only read-only members are called on it and on the objects it returns.
    let policy: INetFwPolicy2 =
        unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER) }.map_err(com)?;
    // SAFETY: a read-only getter on the policy object created above.
    let rules = unsafe { policy.Rules() }.map_err(com)?;
    // SAFETY: a read-only enumerator over the live rule collection, converted to `IEnumVARIANT`
    // before use.
    let unknown = unsafe { rules._NewEnum() }.map_err(com)?;
    let enumerator: IEnumVARIANT = unknown.cast().map_err(com)?;
    let mut scanned = 0_usize;
    loop {
        if Instant::now() >= deadline {
            return Err(ScanError::Deadline);
        }
        // A `VARIANT` clears what `Next` fills in when it drops, on every path.
        let mut slot = [VARIANT::default()];
        let mut fetched = 0_u32;
        // SAFETY: one writable VARIANT slot and a valid out-count.
        unsafe { enumerator.Next(&mut slot, &mut fetched) }
            .ok()
            .map_err(com)?;
        if fetched == 0 {
            return Ok(None);
        }
        scanned += 1;
        if scanned > MAX_SCANNED {
            return Err(ScanError::TooMany);
        }
        let dispatch = IDispatch::try_from(&slot[0]).map_err(com)?;
        let rule: INetFwRule = dispatch.cast().map_err(com)?;
        // SAFETY: a read-only getter on a rule object from the collection read above.
        let name = unsafe { rule.Name() }.map_err(com)?;
        if name.len() > MAX_RULE_NAME_UNITS || !has_rule_prefix(&name) {
            continue;
        }
        // Only a rule of the Crosspane family is converted to text.
        let Some(id) = String::from_utf16(&name)
            .ok()
            .and_then(|name| install_id_from_rule_name(&name))
        else {
            continue;
        };
        // SAFETY: a read-only getter on the same rule object.
        let application = unsafe { rule.ApplicationName() }.map_err(com)?;
        // The helper's own comparison: it is what decides whether another name already covers
        // the agent.
        if program.same_path(&String::from_utf16_lossy(&application)) {
            return Ok(Some(id));
        }
    }
}

// --- observing -------------------------------------------------------------------------------

/// The helper files kept unreplaceable while a status is read. Only held, never read.
#[derive(Debug)]
#[allow(dead_code)]
enum Held {
    Kit(KitHold),
    File(fs::File),
}

/// The helper image to ask for the status: the installed one if the whole placed kit matches its
/// pins, else the helper in the payload folder if it matches its pin.
fn status_helper(
    paths: &Paths,
    payload: Option<&Path>,
    kit: &KitManifest,
) -> Result<(PathBuf, Held), String> {
    if let Ok((image, hold)) = elevated_kit::hold_placed(&paths.install, kit) {
        return Ok((image, Held::Kit(hold)));
    }
    let Some(payload) = payload else {
        return Err(NOT_INSTALLED_YET.to_owned());
    };
    // The launcher accepts an absolute path only, so a relative `--payload` is made absolute.
    let payload = std::path::absolute(payload).unwrap_or_else(|_| payload.to_path_buf());
    let image = payload.join(PayloadFile::Helper.relative());
    let file = hold_pinned_helper(&image, kit.pin(KitFile::Helper))
        .ok_or_else(|| PAYLOAD_DAMAGED.to_owned())?;
    Ok((image, Held::File(file)))
}

/// Opens the helper at `path` for reading only, sharing READ, and checks its size and digest
/// through that same handle. A reparse point, a directory or a mismatch is `None`. The caller
/// keeps the handle for as long as the file must not change.
fn hold_pinned_helper(path: &Path, pin: &KitPin) -> Option<fs::File> {
    debug_assert_eq!(PayloadFile::Helper.relative(), HELPER_IMAGE);
    let metadata = fs::symlink_metadata(path).ok()?;
    if !plain_file(&metadata) || metadata.len() != pin.size {
        return None;
    }
    // The link itself is opened, not its target, so a reparse point swapped in after the check
    // above is seen on the handle below.
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    let opened = file.metadata().ok()?;
    if !plain_file(&opened) || opened.len() != pin.size {
        return None;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(pin.size).ok()?);
    (&file)
        .take(pin.size.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    let actual = digest(&SHA256, &bytes);
    (bytes.len() as u64 == pin.size && actual.as_ref() == pin.sha256.as_slice()).then_some(file)
}

fn plain_file(metadata: &Metadata) -> bool {
    metadata.file_type().is_file()
        && (metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) == 0
}

// --- running ---------------------------------------------------------------------------------

/// Whether the plan names the installed agent, so the rule is for the program Crosspane runs.
fn plans_installed_agent(paths: &Paths, verb: &Verb) -> bool {
    let agent = paths.file(PayloadFile::Agent);
    verb.scope()
        .is_some_and(|scope| scope.program.same_path(&agent.to_string_lossy()))
}

/// The helper's log-only journal: every call becomes one setup log line, and none can fail.
#[derive(Debug)]
struct LogJournal;

impl ElevatedJournal for LogJournal {
    fn record(&mut self, entry: &JournalEntry) -> Result<(), ElevatedError> {
        setup_note!(
            "elevated {:?} {:?} exit={:?}",
            entry.phase,
            entry.verb,
            entry.exit
        );
        Ok(())
    }
}

/// `CoInitializeEx(STA)` on this thread, balanced on drop. `ShellExecuteExW` needs an STA.
#[derive(Debug)]
struct StaApartment(PhantomData<Rc<()>>);

impl StaApartment {
    fn enter() -> Option<Self> {
        // SAFETY: initializes COM only for the calling thread. `Drop` balances a success (S_OK or
        // S_FALSE), and the guard can't leave this thread.
        let code = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if code.is_err() {
            setup_note!("com error 0x{:08X} entering an apartment", code.0 as u32);
            return None;
        }
        Some(Self(PhantomData))
    }
}

impl Drop for StaApartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful `CoInitializeEx` on this same thread.
        unsafe { CoUninitialize() };
    }
}

/// Runs the helper on the calling (worker) thread inside its own STA.
fn run_on_sta(
    helper: &ElevatedHelper,
    verb: &Verb,
    consent: &Consent,
) -> Result<LaunchOutcome, NativeError> {
    let Some(_apartment) = StaApartment::enter() else {
        return Err(NativeError::Unavailable);
    };
    helper.run(verb, consent, &mut LogJournal)
}

/// Maps what the launcher established to the outcome of the administrator step. Every part must be
/// verified (or need only a restart) for `Configured`.
fn outcome_of(verb: &Verb, launched: LaunchOutcome) -> ElevatedOutcome {
    let (result, after) = match launched {
        LaunchOutcome::Declined => return ElevatedOutcome::Declined,
        LaunchOutcome::Verified { outcome, after } => (
            StepResult::launched(verb, &[(verb.clone(), Some(outcome))], Some(&after)),
            Some(after),
        ),
        LaunchOutcome::Unverified { outcome, after } => (
            StepResult::launched(verb, &[(verb.clone(), outcome)], after.as_ref()),
            after,
        ),
        LaunchOutcome::Parts { outcomes, after } => {
            (StepResult::launched(verb, &outcomes, after.as_ref()), after)
        }
    };
    let done = !result.parts.is_empty()
        && result
            .parts
            .iter()
            .all(|(_, part)| matches!(part, PartResult::Verified | PartResult::RebootRequired));
    let restart = result
        .parts
        .iter()
        .any(|(_, part)| *part == PartResult::RebootRequired);
    if done {
        // A step on the driver alone reads its state without the firewall rule (`not checked`), so
        // it says what it did instead of a summary with a gap in it.
        let text = match (&after, restart) {
            (Some(after), false) if after.firewall.state != FirewallState::NotRequested => {
                summary(after)
            }
            _ => report_lines(&result).join(" "),
        };
        return ElevatedOutcome::Configured(text);
    }
    let mut lines = report_lines(&result);
    if lines.is_empty() {
        lines.push(NOT_RUN_MESSAGE.to_owned());
    }
    let unknown = result
        .parts
        .iter()
        .any(|(_, part)| *part == PartResult::Unknown);
    if let (true, Some(after)) = (unknown, &after) {
        lines.push(summary(after));
    }
    ElevatedOutcome::Failed(lines.join(" "))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crosspane_installer_core::elevated::{
        Outcome, VerbName,
        status::{DeviceStatus, DriverStatus, FirewallStatus},
        step::PREVIEW_HEADER,
    };

    fn report(firewall: FirewallState, driver: DriverState) -> StatusReport {
        StatusReport {
            schema: 1,
            elevated: false,
            driver: DriverStatus {
                state: driver,
                packages: Vec::new(),
                devices: Vec::<DeviceStatus>::new(),
            },
            firewall: FirewallStatus {
                state: firewall,
                family_count: 0,
                local_rules_apply: None,
            },
        }
    }

    fn scope_for_tests() -> RuleScope {
        RuleScope {
            id: InstallId::parse("eb281d43dffa02d3e3370970828abfe2").unwrap(),
            program: AgentProgram::parse(
                r"C:\Users\someone\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
            )
            .unwrap(),
        }
    }

    use DriverState as D;
    use FirewallState as F;

    #[test]
    fn setup_needed_truth_table() {
        let table = [
            (F::Present, D::Installed, false),
            (F::Missing, D::Installed, true),
            (F::Mismatch, D::Installed, true),
            (F::Present, D::Absent, true),
            (F::Present, D::PackageOnly, true),
            (F::Present, D::DeviceWithoutDriver, true),
            (F::Missing, D::Absent, true),
            (F::Mismatch, D::Mismatch, true),
            // States that couldn't be read or aren't Crosspane's are not a reason to ask.
            (F::Present, D::Mismatch, false),
            (F::Present, D::Unavailable, false),
            (F::Unavailable, D::Installed, false),
            (F::Unavailable, D::Unavailable, false),
            (F::NotRequested, D::Installed, false),
        ];
        for (firewall, driver, want) in table {
            assert_eq!(
                setup_needed(&report(firewall, driver)),
                want,
                "firewall {firewall:?}, driver {driver:?}"
            );
        }
    }

    #[test]
    fn teardown_needed_truth_table() {
        let table = [
            (F::Present, D::Installed, true),
            (F::Missing, D::Installed, true),
            (F::Present, D::Absent, true),
            (F::Mismatch, D::Absent, true),
            (F::Missing, D::PackageOnly, true),
            (F::Missing, D::DeviceWithoutDriver, true),
            (F::Missing, D::Mismatch, true),
            (F::Missing, D::Absent, false),
            (F::Missing, D::Unavailable, false),
            (F::Unavailable, D::Unavailable, false),
            (F::NotRequested, D::Absent, false),
        ];
        for (firewall, driver, want) in table {
            assert_eq!(
                teardown_needed(&report(firewall, driver)),
                want,
                "firewall {firewall:?}, driver {driver:?}"
            );
        }
    }

    /// The kind of verb a table row expects, without its scope.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Want {
        Nothing,
        Firewall,
        Driver,
        Both,
    }

    fn shape(verb: Option<Verb>, both: VerbName, firewall: VerbName, driver: VerbName) -> Want {
        match verb {
            None => Want::Nothing,
            Some(verb) if verb.name() == both => Want::Both,
            Some(verb) if verb.name() == firewall => Want::Firewall,
            Some(verb) if verb.name() == driver => Want::Driver,
            Some(other) => panic!("unexpected verb {other:?}"),
        }
    }

    #[test]
    fn setup_verb_names_only_what_is_missing() {
        let table = [
            (F::Present, D::Installed, Want::Nothing),
            // The live state that prompted this: the rule needs widening, the driver is fine.
            (F::Mismatch, D::Installed, Want::Firewall),
            (F::Missing, D::Installed, Want::Firewall),
            (F::Present, D::Absent, Want::Driver),
            (F::Present, D::PackageOnly, Want::Driver),
            (F::Present, D::DeviceWithoutDriver, Want::Driver),
            (F::Missing, D::Absent, Want::Both),
            (F::Mismatch, D::PackageOnly, Want::Both),
            (F::Missing, D::DeviceWithoutDriver, Want::Both),
            // States that couldn't be read or aren't Crosspane's are not a reason to ask.
            (F::Present, D::Mismatch, Want::Nothing),
            (F::Missing, D::Mismatch, Want::Firewall),
            (F::Present, D::Unavailable, Want::Nothing),
            (F::Unavailable, D::Installed, Want::Nothing),
            (F::Unavailable, D::Absent, Want::Driver),
            (F::Unavailable, D::Unavailable, Want::Nothing),
            (F::NotRequested, D::Installed, Want::Nothing),
            (F::NotRequested, D::Absent, Want::Driver),
        ];
        for (firewall, driver, want) in table {
            let report = report(firewall, driver);
            let verb = setup_verb(&report, scope_for_tests());
            assert_eq!(
                verb.is_some(),
                setup_needed(&report),
                "firewall {firewall:?}, driver {driver:?}"
            );
            assert_eq!(
                shape(
                    verb,
                    VerbName::Setup,
                    VerbName::AddFirewall,
                    VerbName::InstallDriver
                ),
                want,
                "firewall {firewall:?}, driver {driver:?}"
            );
        }
    }

    #[test]
    fn teardown_verb_names_only_what_is_there() {
        let table = [
            (F::Present, D::Installed, Want::Both),
            (F::Mismatch, D::Mismatch, Want::Both),
            (F::Present, D::PackageOnly, Want::Both),
            (F::Present, D::Absent, Want::Firewall),
            (F::Mismatch, D::Absent, Want::Firewall),
            (F::Present, D::Unavailable, Want::Firewall),
            (F::Missing, D::Installed, Want::Driver),
            (F::Missing, D::PackageOnly, Want::Driver),
            (F::Missing, D::DeviceWithoutDriver, Want::Driver),
            (F::Missing, D::Mismatch, Want::Driver),
            (F::Unavailable, D::Installed, Want::Driver),
            (F::NotRequested, D::Installed, Want::Driver),
            (F::Missing, D::Absent, Want::Nothing),
            (F::Missing, D::Unavailable, Want::Nothing),
            (F::Unavailable, D::Unavailable, Want::Nothing),
            (F::NotRequested, D::Absent, Want::Nothing),
        ];
        for (firewall, driver, want) in table {
            let report = report(firewall, driver);
            let verb = teardown_verb(&report, scope_for_tests());
            assert_eq!(
                verb.is_some(),
                teardown_needed(&report),
                "firewall {firewall:?}, driver {driver:?}"
            );
            assert_eq!(
                shape(
                    verb,
                    VerbName::Teardown,
                    VerbName::RemoveFirewall,
                    VerbName::RemoveDriver
                ),
                want,
                "firewall {firewall:?}, driver {driver:?}"
            );
        }
    }

    #[test]
    fn the_chosen_verbs_carry_the_scope_only_where_the_firewall_is_involved() {
        let scope = scope_for_tests();
        let both = report(F::Missing, D::Absent);
        assert_eq!(
            setup_verb(&both, scope.clone()),
            Some(Verb::Setup(scope.clone()))
        );
        assert_eq!(
            setup_verb(&report(F::Missing, D::Installed), scope.clone()),
            Some(Verb::AddFirewall(scope.clone()))
        );
        assert_eq!(
            setup_verb(&report(F::Present, D::Absent), scope.clone()),
            Some(Verb::InstallDriver)
        );
        let there = report(F::Present, D::Installed);
        assert_eq!(
            teardown_verb(&there, scope.clone()),
            Some(Verb::Teardown(scope.clone()))
        );
        assert_eq!(
            teardown_verb(&report(F::Present, D::Absent), scope.clone()),
            Some(Verb::RemoveFirewall(scope))
        );
        assert_eq!(
            teardown_verb(&report(F::Missing, D::Installed), scope_for_tests()),
            Some(Verb::RemoveDriver)
        );
    }

    #[test]
    fn every_chosen_verb_makes_a_plan_that_shows_only_its_own_lines() {
        let scope = scope_for_tests();
        let every = [
            Verb::Setup(scope.clone()),
            Verb::AddFirewall(scope.clone()),
            Verb::InstallDriver,
            Verb::Teardown(scope.clone()),
            Verb::RemoveFirewall(scope),
            Verb::RemoveDriver,
        ];
        for verb in every {
            let planned = plan(verb.clone()).unwrap();
            assert_eq!(planned.verb(), &verb);
            let block = planned.preview_block();
            assert!(block.starts_with(PREVIEW_HEADER), "{verb:?}");
            assert!(block.len() <= 1_800, "{verb:?}: {} bytes", block.len());
            // The consent is for exactly this verb.
            assert_eq!(planned.consent().unwrap().action(), &verb);
            let mentions_driver = block.contains("display driver");
            let mentions_firewall = block.contains("Firewall rule");
            let touches_driver = verb
                .parts()
                .iter()
                .any(|part| matches!(part, Verb::InstallDriver | Verb::RemoveDriver));
            let touches_firewall = verb
                .parts()
                .iter()
                .any(|part| matches!(part, Verb::AddFirewall(_) | Verb::RemoveFirewall(_)));
            assert_eq!(mentions_driver, touches_driver, "{verb:?}: {block}");
            assert_eq!(mentions_firewall, touches_firewall, "{verb:?}: {block}");
        }
    }

    #[test]
    fn a_consent_covers_a_wanted_step_only_when_they_share_a_part() {
        let scope = scope_for_tests();
        let setup = Verb::Setup(scope.clone());
        let add_firewall = Verb::AddFirewall(scope.clone());
        let teardown = Verb::Teardown(scope.clone());
        assert!(shares_a_part(&setup, &setup));
        assert!(shares_a_part(&setup, &add_firewall));
        assert!(shares_a_part(&setup, &Verb::InstallDriver));
        assert!(shares_a_part(&add_firewall, &setup));
        assert!(shares_a_part(&Verb::InstallDriver, &setup));
        assert!(shares_a_part(&teardown, &Verb::RemoveDriver));
        assert!(shares_a_part(&Verb::RemoveFirewall(scope), &teardown));
        // The firewall rule was agreed to, but only the driver is wanted now.
        assert!(!shares_a_part(&add_firewall, &Verb::InstallDriver));
        assert!(!shares_a_part(&Verb::InstallDriver, &add_firewall));
        // Setting up and tearing down are different things.
        assert!(!shares_a_part(&setup, &teardown));
        assert!(!shares_a_part(&Verb::RemoveDriver, &Verb::InstallDriver));
    }

    #[test]
    fn declining_is_explained_for_what_was_asked() {
        let scope = scope_for_tests();
        assert_eq!(
            without_setup_note(&Verb::AddFirewall(scope.clone())),
            "Until the firewall rule is added, other computers may not reach this PC."
        );
        assert_eq!(
            without_setup_note(&Verb::InstallDriver),
            "Without the display driver, Crosspane mirrors windows in place."
        );
        assert_eq!(
            without_setup_note(&Verb::Setup(scope)),
            INSTALL_WITHOUT_SETUP
        );
        // The core sentence for both is made of these two halves.
        for half in [
            without_setup_note(&Verb::InstallDriver),
            without_setup_note(&Verb::AddFirewall(scope_for_tests())),
        ] {
            let lower = half.trim_end_matches('.').to_lowercase();
            assert!(
                INSTALL_WITHOUT_SETUP.to_lowercase().contains(&lower),
                "{half}"
            );
        }
    }

    #[test]
    fn the_step_line_says_what_needs_approval_in_plain_words() {
        let table = [
            (
                F::Missing,
                D::Installed,
                "Needs administrator approval to allow Crosspane through the firewall.",
            ),
            (
                F::Mismatch,
                D::Installed,
                "Needs administrator approval to allow Crosspane through the firewall.",
            ),
            (
                F::Present,
                D::Absent,
                "Needs administrator approval to add the display driver.",
            ),
            (
                F::Missing,
                D::Absent,
                "Needs administrator approval to allow Crosspane through the firewall and add the display driver.",
            ),
            (
                F::Present,
                D::Installed,
                "Firewall rule and display driver are set up.",
            ),
            // Nothing to approve, but not set up either: the core words for the state.
            (
                F::Present,
                D::Mismatch,
                "Firewall rule: present. Display driver: different from Crosspane's driver.",
            ),
            (
                F::Unavailable,
                D::Unavailable,
                "Firewall rule: unreadable. Display driver: unreadable.",
            ),
        ];
        for (firewall, driver, want) in table {
            assert_eq!(
                plain_status(&report(firewall, driver)),
                want,
                "firewall {firewall:?}, driver {driver:?}"
            );
        }
    }

    #[test]
    fn the_step_line_is_never_a_bare_checked() {
        let states = [
            F::Present,
            F::Missing,
            F::Mismatch,
            F::Unavailable,
            F::NotRequested,
        ];
        let drivers = [
            D::Absent,
            D::PackageOnly,
            D::DeviceWithoutDriver,
            D::Installed,
            D::Mismatch,
            D::Unavailable,
        ];
        for firewall in states {
            for driver in drivers {
                let line = plain_status(&report(firewall, driver));
                assert!(line.len() > "Checked.".len(), "{line}");
                assert_ne!(line, "Checked.");
                for banned in [
                    "foreign",
                    "unsettled",
                    "admission",
                    "submission",
                    "epoch",
                    "journal",
                    "outcomeunknown",
                ] {
                    assert!(!line.to_lowercase().contains(banned), "{line}");
                }
            }
        }
    }

    #[test]
    fn a_single_step_that_worked_says_what_it_did() {
        let scope = scope_for_tests();
        // The driver step reads its state without the firewall rule.
        let driver = outcome_of(
            &Verb::InstallDriver,
            LaunchOutcome::Verified {
                outcome: Outcome::Done,
                after: report(F::NotRequested, D::Installed),
            },
        );
        assert_eq!(
            driver,
            ElevatedOutcome::Configured("Crosspane display driver: installed.".to_owned())
        );
        // The firewall step reads both, so the whole state is told.
        let after = report(F::Present, D::Installed);
        let firewall = outcome_of(
            &Verb::AddFirewall(scope),
            LaunchOutcome::Verified {
                outcome: Outcome::Done,
                after: after.clone(),
            },
        );
        assert_eq!(firewall, ElevatedOutcome::Configured(summary(&after)));
    }

    #[test]
    fn a_declined_single_step_is_declined() {
        assert_eq!(
            outcome_of(&Verb::InstallDriver, LaunchOutcome::Declined),
            ElevatedOutcome::Declined
        );
        assert_eq!(
            outcome_of(
                &Verb::AddFirewall(scope_for_tests()),
                LaunchOutcome::Declined
            ),
            ElevatedOutcome::Declined
        );
    }

    #[test]
    fn install_id_comes_from_a_crosspane_rule_name() {
        let id = install_id_from_rule_name(
            "Crosspane.Agent.UDP.Private.eb281d43dffa02d3e3370970828abfe2",
        )
        .unwrap();
        assert_eq!(id.as_str(), "eb281d43dffa02d3e3370970828abfe2");
        // The rule store compares names ASCII case-insensitively; the id keeps its own case.
        let id = install_id_from_rule_name("crosspane.agent.udp.private.AbC-123").unwrap();
        assert_eq!(id.as_str(), "AbC-123");
        assert_eq!(id.rule_name(), "Crosspane.Agent.UDP.Private.AbC-123");
    }

    #[test]
    fn install_id_refuses_other_names() {
        for name in [
            "",
            "Crosspane.Agent.UDP.Private.",
            "Crosspane.Agent.UDP.Public.abc",
            "Crosspane.Agent.UDP.Privat",
            "Other.Agent.UDP.Private.abc",
            // Malformed ids: separator, space, path-like, non-ASCII, over-long.
            "Crosspane.Agent.UDP.Private.abc.def",
            "Crosspane.Agent.UDP.Private.abc def",
            "Crosspane.Agent.UDP.Private.abc\\def",
            "Crosspane.Agent.UDP.Private.abc\u{e9}",
            &format!(
                "Crosspane.Agent.UDP.Private.{}",
                "a".repeat(MAX_INSTALL_ID + 1)
            ),
            // A multi-byte character straddling the end of the prefix must not panic.
            "Crosspane.Agent.UDP.Priva\u{e9}e.abc",
        ] {
            assert!(install_id_from_rule_name(name).is_none(), "{name:?}");
        }
        let longest = format!("Crosspane.Agent.UDP.Private.{}", "a".repeat(MAX_INSTALL_ID));
        assert!(install_id_from_rule_name(&longest).is_some());
        assert_eq!(longest.len(), MAX_RULE_NAME_UNITS);
    }

    #[test]
    fn rule_prefix_is_matched_on_utf16_units() {
        let units = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
        assert!(has_rule_prefix(&units("Crosspane.Agent.UDP.Private.abc")));
        assert!(has_rule_prefix(&units("CROSSPANE.AGENT.UDP.PRIVATE.")));
        assert!(!has_rule_prefix(&units("Crosspane.Agent.UDP.Private")));
        assert!(!has_rule_prefix(&units("Core Networking - DNS (UDP-Out)")));
        assert!(!has_rule_prefix(&units("")));
        // A unit above 0xFF never matches, even if its low byte would.
        let mut lookalike = units("Crosspane.Agent.UDP.Private.");
        lookalike[0] = 0x0143;
        assert!(!has_rule_prefix(&lookalike));
    }

    #[test]
    fn declined_prompt_is_declined() {
        let verb = Verb::Setup(scope_for_tests());
        assert_eq!(
            outcome_of(&verb, LaunchOutcome::Declined),
            ElevatedOutcome::Declined
        );
    }

    #[test]
    fn verified_setup_is_configured_with_the_state_summary() {
        let scope = scope_for_tests();
        let verb = Verb::Setup(scope.clone());
        let after = report(F::Present, D::Installed);
        let launched = LaunchOutcome::Parts {
            outcomes: vec![
                (Verb::AddFirewall(scope), Some(Outcome::Done)),
                (Verb::InstallDriver, Some(Outcome::AlreadyDone)),
            ],
            after: Some(after.clone()),
        };
        assert_eq!(
            outcome_of(&verb, launched),
            ElevatedOutcome::Configured(summary(&after))
        );
        assert_eq!(
            summary(&after),
            "Firewall rule: present. Display driver: installed."
        );
    }

    #[test]
    fn a_driver_that_needs_a_restart_still_configures() {
        let scope = scope_for_tests();
        let verb = Verb::Setup(scope.clone());
        let launched = LaunchOutcome::Parts {
            outcomes: vec![
                (Verb::AddFirewall(scope), Some(Outcome::Done)),
                (Verb::InstallDriver, Some(Outcome::RebootRequired)),
            ],
            after: Some(report(F::Present, D::PackageOnly)),
        };
        let ElevatedOutcome::Configured(text) = outcome_of(&verb, launched) else {
            panic!("a restart is not a failure");
        };
        assert!(text.contains("restart"), "{text}");
    }

    #[test]
    fn a_refused_or_failed_part_is_a_failure_with_plain_text() {
        let scope = scope_for_tests();
        let verb = Verb::Setup(scope.clone());
        let launched = LaunchOutcome::Parts {
            outcomes: vec![
                (Verb::AddFirewall(scope), Some(Outcome::Done)),
                (Verb::InstallDriver, Some(Outcome::Failed)),
            ],
            after: Some(report(F::Present, D::Absent)),
        };
        let ElevatedOutcome::Failed(text) = outcome_of(&verb, launched) else {
            panic!("a failed driver is a failure");
        };
        assert!(
            text.contains("Windows Defender Firewall rule: added."),
            "{text}"
        );
        assert!(
            text.contains("Crosspane display driver: could not be changed"),
            "{text}"
        );
    }

    #[test]
    fn an_unknown_exit_is_a_failure_that_names_the_current_state() {
        let scope = scope_for_tests();
        let verb = Verb::Setup(scope);
        let launched = LaunchOutcome::Parts {
            outcomes: vec![(Verb::InstallDriver, None)],
            after: Some(report(F::Missing, D::Absent)),
        };
        let ElevatedOutcome::Failed(text) = outcome_of(&verb, launched) else {
            panic!("an unknown exit is not success");
        };
        assert!(
            text.contains("Firewall rule: missing. Display driver: not installed."),
            "{text}"
        );
    }

    #[test]
    fn an_unreadable_result_is_never_configured() {
        let verb = Verb::Teardown(scope_for_tests());
        let launched = LaunchOutcome::Parts {
            outcomes: Vec::new(),
            after: None,
        };
        assert!(matches!(
            outcome_of(&verb, launched),
            ElevatedOutcome::Failed(_)
        ));
    }

    #[test]
    fn plans_carry_the_consent_lines_for_setup_and_teardown() {
        let setup = plan(Verb::Setup(scope_for_tests())).unwrap();
        assert!(setup.preview_block().starts_with(PREVIEW_HEADER));
        assert!(plan(Verb::Teardown(scope_for_tests())).is_ok());
        assert!(plan(Verb::Status(None)).is_err());
    }
}
