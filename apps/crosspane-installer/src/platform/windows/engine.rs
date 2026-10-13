//! The lifecycle engine: plan, converge, start and uninstall, plus the user-facing texts.
//!
//! One idempotent `converge` brings the machine to the desired state; running it again always
//! converges, and nothing it persists ever gates a later run. `uninstall` mirrors it.

use super::{
    agent_pipe,
    elevated::{self, ElevatedOutcome},
    error::{SetupError, SetupResult, note_io},
    files::{self, FileState, FilesOutcome, Inventory, Sources},
    legacy::{self, Backup},
    lock::{self, OperationLock},
    paths::{self, Paths, PayloadFile},
    process,
    setup_log::setup_note,
    shell,
    state::{self, Installed},
    stop, supervisor,
    task::{self, TaskHealth},
};
use crosspane_installer_core::elevated::{
    RuleScope, Verb,
    kit::KitManifest,
    status::StatusReport,
    step::{ElevatedPlan, INSTALL_DECLINE_NOTE},
};
use std::{
    fs,
    io::Read as _,
    os::windows::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::ERROR_ACCESS_DENIED,
    System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
    },
};

/// How long the agent has to answer after it was started.
const AGENT_WAIT: Duration = Duration::from_secs(45);
/// The longest silence between two progress lines while waiting.
const HEARTBEAT: Duration = Duration::from_secs(15);
/// How long a running supervisor's agent has to answer before it counts as hung.
const HUNG_PROBE: Duration = Duration::from_secs(10);
/// How long the sign-in task has to start the supervisor before it is started directly.
const SUPERVISOR_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(250);
/// The longest the identity erase may run.
const ERASE_WAIT: Duration = Duration::from_secs(30);
/// How long a delete is retried while another program (for example a virus scanner) holds a file.
const REMOVE_WAIT: Duration = Duration::from_secs(5);
/// Backup folders kept after an operation.
const BACKUPS_KEPT: usize = 5;
/// Win32 `ERROR_ACCESS_DENIED` and `ERROR_SHARING_VIOLATION`: another program holds the file.
const BUSY_CODES: [i32; 2] = [5, 32];

/// Receives plain progress lines from long operations.
pub(crate) trait Progress {
    fn step(&self, text: &str);
}

/// Whether, and with what consent, the administrator step may run.
#[derive(Clone, Debug)]
pub(crate) enum ElevatedPolicy {
    Never,
    IfNeeded,
    Consented(ElevatedPlan),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Install,
    Repair,
    CompleteReinstall,
}

#[derive(Clone, Debug)]
pub(crate) struct ConvergeOptions {
    pub payload: Option<PathBuf>,
    pub route: Route,
    pub elevated: ElevatedPolicy,
}

/// What a converge would do, computed without changing anything.
#[derive(Clone, Debug)]
pub(crate) struct ConvergePlan {
    pub copies: Vec<PayloadFile>,
    pub unrestorable: Vec<PayloadFile>,
    pub restart: bool,
    pub legacy: bool,
    pub task: bool,
    pub shell: bool,
    pub start: bool,
    pub elevated: Option<ElevatedPlan>,
    pub elevated_note: Option<String>,
}

/// The decline sentence of an install preview for a firewall rule on its own: what declining
/// leaves, in the same words as the line shown after a declined prompt.
const DECLINE_NOTE_FIREWALL: &str = "If you decline, Crosspane is still installed, but other computers may not reach this PC until the firewall rule is added.";
/// The same for the display driver on its own.
const DECLINE_NOTE_DRIVER: &str = "If you decline, Crosspane is still installed and mirrors windows in place without the display driver.";

/// The decline sentence that follows the administrator block of `verb`: only the half that was
/// asked for, and the core sentence when it was both.
fn decline_note(verb: &Verb) -> &'static str {
    match verb {
        Verb::AddFirewall(_) => DECLINE_NOTE_FIREWALL,
        Verb::InstallDriver => DECLINE_NOTE_DRIVER,
        _ => INSTALL_DECLINE_NOTE,
    }
}

/// Whether any Crosspane program is running now: one from the install folder, or the sign-in
/// supervisor. When the folders can't be resolved there is no telling, so it counts as running.
fn crosspane_running() -> bool {
    match Paths::current() {
        Ok(paths) => {
            !process::under(std::slice::from_ref(&paths.install)).is_empty()
                || supervisor::running()
        }
        Err(_) => true,
    }
}

impl ConvergePlan {
    /// The preview text, plus the elevated `preview_block` and its decline sentence when needed.
    ///
    /// The core `PREVIEW_HEADER` appears only when an administrator step is planned: that is what
    /// keeps the wizard from approving a prompt on its own. The "Stop Crosspane" line appears only
    /// when something is running to stop, so it looks at the machine, but only when the plan would
    /// stop Crosspane at all.
    pub(crate) fn preview(&self, route: Route) -> String {
        let running = self.restart && crosspane_running();
        self.preview_with(route, running)
    }

    /// `preview` for a known `running` state (whether any Crosspane program is running now).
    fn preview_with(&self, route: Route, running: bool) -> String {
        let mut lines: Vec<String> = Vec::new();
        lines.push(
            match route {
                Route::Install => "Install Crosspane for this account.",
                Route::Repair => "Repair Crosspane.",
                Route::CompleteReinstall => "Reinstall all of Crosspane's files.",
            }
            .to_owned(),
        );
        if self.restart && running {
            lines.push(
                if self.copies.is_empty() {
                    "Stop Crosspane for a moment."
                } else {
                    "Stop Crosspane while its files are replaced."
                }
                .to_owned(),
            );
        }
        if !self.copies.is_empty() {
            lines.push(match (route, self.copies.len()) {
                (Route::Repair, _) => "Restore missing or changed files.".to_owned(),
                (_, 1) => "Replace 1 file.".to_owned(),
                (_, count) => format!("Replace {count} files."),
            });
        }
        if self.legacy {
            lines.push("Clean up files left by an older setup.".to_owned());
        }
        if self.task {
            lines.push("Register Crosspane to start when you sign in.".to_owned());
        }
        if self.shell {
            lines.push("Add Crosspane to the Start menu and Apps & features.".to_owned());
        }
        if self.start {
            lines.push("Start Crosspane.".to_owned());
        }
        if !self.unrestorable.is_empty() {
            lines.push(text::NEEDS_DOWNLOADED_INSTALLER.to_owned());
        }
        if let Some(plan) = &self.elevated {
            lines.push(plan.preview_block());
            lines.push(decline_note(plan.verb()).to_owned());
        }
        if let Some(note) = &self.elevated_note {
            lines.push(note.clone());
        }
        lines.join("\n")
    }
}

/// The payload folder to install from: the caller's, else the one beside this installer.
fn payload_of(options: &ConvergeOptions) -> Option<PathBuf> {
    options
        .payload
        .clone()
        .or_else(paths::payload_beside_current_exe)
}

/// Read-only. `restart = route != Install || copies non-empty || legacy || (processes under
/// install && !supervisor::running())`. `CompleteReinstall` without a payload →
/// `Err(before(text::NEEDS_DOWNLOADED_INSTALLER))`.
pub(crate) fn plan(
    paths: &Paths,
    inventory: &Inventory,
    options: &ConvergeOptions,
) -> SetupResult<ConvergePlan> {
    let payload = payload_of(options);
    let sources = files::sources(payload.as_deref(), inventory)?;
    plan_with(paths, inventory, options, payload.as_deref(), &sources)
}

/// `plan` for sources that were already verified, so `converge` checks them only once.
fn plan_with(
    paths: &Paths,
    inventory: &Inventory,
    options: &ConvergeOptions,
    payload: Option<&Path>,
    sources: &Sources,
) -> SetupResult<ConvergePlan> {
    let complete = options.route == Route::CompleteReinstall;
    let has_payload = sources.has_payload();
    if complete && !has_payload {
        return Err(SetupError::before(text::NEEDS_DOWNLOADED_INSTALLER));
    }

    // The installed installer never replaces itself while it is the one running.
    let installed_installer = paths.file(PayloadFile::Installer);
    let running_installed =
        std::env::current_exe().is_ok_and(|exe| paths::same_path(&exe, &installed_installer));
    let mut copies = Vec::new();
    let mut unrestorable = Vec::new();
    for (file, state) in files::inspect(paths, inventory) {
        if (!complete && state == FileState::Ok)
            || (file == PayloadFile::Installer && running_installed)
        {
            continue;
        }
        // Without a payload folder only the running installer itself can be put back.
        if has_payload || file == PayloadFile::Installer {
            copies.push(file);
        } else {
            unrestorable.push(file);
        }
    }

    let legacy_found = !legacy::items(paths).is_empty();
    let task_needed =
        task::inspect(paths) != TaskHealth::Healthy || options.route != Route::Install;
    let shell_state = shell::observe(paths);
    let shell_needed = !(shell_state.start_menu && shell_state.uninstall_entry);
    let restart = options.route != Route::Install
        || !copies.is_empty()
        || legacy_found
        || (!process::under(std::slice::from_ref(&paths.install)).is_empty()
            && !supervisor::running());

    let (elevated, elevated_note) = match &options.elevated {
        ElevatedPolicy::Consented(plan) => (Some(plan.clone()), None),
        policy => match observe_verb(paths, payload, None, elevated::setup_verb) {
            Err(reason) => (None, Some(reason)),
            Ok(None) => (None, None),
            Ok(Some(verb)) => match policy {
                ElevatedPolicy::IfNeeded => (Some(elevated::plan(verb)?), None),
                _ => (None, Some(approval_pending(&verb).to_owned())),
            },
        },
    };

    Ok(ConvergePlan {
        copies,
        unrestorable,
        restart,
        legacy: legacy_found,
        task: task_needed,
        shell: shell_needed,
        start: true,
        elevated,
        elevated_note,
    })
}

/// Shown when the administrator step is skipped but would be needed.
const APPROVAL_PENDING: &str = "The firewall rule and display driver still need administrator approval. Choose Repair to approve.";
const APPROVAL_PENDING_FIREWALL: &str =
    "The firewall rule still needs administrator approval. Choose Repair to approve.";
const APPROVAL_PENDING_DRIVER: &str =
    "The display driver still needs administrator approval. Choose Repair to approve.";

/// The note for a skipped administrator step `verb`, naming only what it would have done.
fn approval_pending(verb: &Verb) -> &'static str {
    match verb {
        Verb::AddFirewall(_) => APPROVAL_PENDING_FIREWALL,
        Verb::InstallDriver => APPROVAL_PENDING_DRIVER,
        _ => APPROVAL_PENDING,
    }
}

/// Reads the helper's status for `scope` (found when `None`) and returns the administrator step it
/// calls for, as `pick` chooses it, or `None` when nothing is called for. The error is plain text
/// for the person using setup.
fn observe_verb(
    paths: &Paths,
    payload: Option<&Path>,
    scope: Option<RuleScope>,
    pick: fn(&StatusReport, RuleScope) -> Option<Verb>,
) -> Result<Option<Verb>, String> {
    let scope = match scope {
        Some(scope) => scope,
        None => elevated::scope(paths).map_err(|error| error.text)?,
    };
    let report = elevated::observe(paths, payload, &scope)?;
    Ok(pick(&report, scope))
}

/// Whether `verb` changes the display driver. The driver has to be in place before the agent
/// starts, so only a step that leaves it alone can wait until Crosspane is running.
fn touches_driver(verb: &Verb) -> bool {
    verb.parts()
        .iter()
        .any(|part| matches!(part, Verb::InstallDriver | Verb::RemoveDriver))
}

/// What a converge does with the sign-in task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskAction {
    /// Register (or re-register) it.
    Register,
    /// Leave it as it is.
    Keep,
    /// Delete it: the task would run a program that failed its integrity check.
    Remove,
}

/// The task runs the supervisor (the installer), which runs the agent, and neither is checked at
/// start, so the task stays only while both programs match their approved files. Otherwise it is
/// re-registered whenever it is unhealthy, and on every route except a plain install.
pub(crate) fn task_action(programs_ok: bool, health: TaskHealth, route: Route) -> TaskAction {
    if !programs_ok {
        TaskAction::Remove
    } else if health != TaskHealth::Healthy || route != Route::Install {
        TaskAction::Register
    } else {
        TaskAction::Keep
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ConvergeReport {
    pub files_ok: bool,
    pub task_ok: bool,
    pub agent_answering: bool,
    pub elevated: ElevatedOutcome,
    pub lines: Vec<String>,
}

impl ConvergeReport {
    /// `files_ok && task_ok && agent_answering`
    pub(crate) fn ok(&self) -> bool {
        self.files_ok && self.task_ok && self.agent_answering
    }
}

/// Moves this process out of the install folder if its current directory is in it. A process
/// whose current directory is the program folder (the Start menu's "Crosspane Setup" shortcut used
/// to start there) keeps Windows from deleting or renaming that folder, and the operation would
/// report the program files as left behind forever. Best effort: a failure is only logged.
fn leave_install_dir(paths: &Paths) {
    let inside = match std::env::current_dir() {
        Ok(dir) => paths::is_under(&dir, &paths.install),
        // Unknown, and moving is harmless.
        Err(_) => true,
    };
    if !inside {
        return;
    }
    let candidates = exit_candidates(
        paths,
        std::env::var_os("SystemRoot").map(PathBuf::from),
        std::env::temp_dir(),
    );
    for dir in candidates {
        if fs::create_dir_all(&dir).is_ok() && std::env::set_current_dir(&dir).is_ok() {
            setup_note!("moved this process out of the program folder");
            return;
        }
    }
    setup_note!("couldn't move this process out of the program folder");
}

/// The folders to move to, best first, none inside the install folder. Folders that an operation
/// may delete itself come last: the state folder goes away with "Also remove settings and
/// pairings", and a process standing in a folder keeps it from being deleted.
fn exit_candidates(paths: &Paths, system_root: Option<PathBuf>, temp: PathBuf) -> Vec<PathBuf> {
    system_root
        .into_iter()
        .chain([temp, paths.state.clone()])
        .filter(|dir| !paths::is_under(dir, &paths.install))
        .collect()
}

/// leave the install folder → lock → sources → plan → [refuse an install that has files it can't
/// restore] → [stop] → `legacy::clean` → `files::place` →
/// task (if !Healthy || route != Install) → `shell::ensure` → elevated per policy (when it
/// installs or removes the driver) → `state::write` (only if `files_ok && task_ok`) →
/// `legacy::prune(5)` → unlock → start (`task::run`, fallback direct spawn) → wait ≤45 s →
/// elevated per policy (when it leaves the driver alone, so Crosspane isn't down while the
/// approval prompt waits).
/// Idempotent; reads no prior state. The lock is released before the start: starting changes no
/// files, and the supervisor gives up on an agent that fails while the lock exists.
pub(crate) fn converge(
    paths: &Paths,
    inventory: &Inventory,
    options: &ConvergeOptions,
    progress: &dyn Progress,
) -> SetupResult<ConvergeReport> {
    leave_install_dir(paths);
    let lock = OperationLock::acquire()?;
    let route = options.route;
    let payload = payload_of(options);
    let sources = files::sources(payload.as_deref(), inventory)?;
    let plan = plan_with(paths, inventory, options, payload.as_deref(), &sources)?;
    setup_note!(
        "converge: route={route:?} copies={} unrestorable={} restart={} legacy={} task={} shell={}",
        plan.copies.len(),
        plan.unrestorable.len(),
        plan.restart,
        plan.legacy,
        plan.task,
        plan.shell
    );
    // Before anything is stopped or changed: Crosspane would be stopped, only some of its files
    // replaced, and never started again.
    if refuses_install(route, &plan) {
        setup_note!(
            "converge: install refused, {} files cannot be restored here",
            plan.unrestorable.len()
        );
        return Err(SetupError::before(text::NEEDS_DOWNLOADED_INSTALLER));
    }
    let mut lines: Vec<String> = Vec::new();

    if plan.restart {
        let stopped = stop::stop_all(paths, progress);
        setup_note!(
            "converge: stop graceful={} remaining={}",
            stopped.graceful,
            stopped.remaining.len()
        );
        if !stopped.remaining.is_empty() {
            return Err(SetupError::after(format!(
                "{} Still running: {}.",
                text::STILL_RUNNING,
                stopped.remaining.join(", ")
            )));
        }
    }

    let mut backup = Backup::new(paths);
    let leftovers = legacy::clean(paths, &mut backup);
    if !leftovers.is_empty() {
        setup_note!("converge: {} old items could not be moved", leftovers.len());
        lines.push(format!("Couldn't move aside: {}.", leftovers.join(", ")));
    }

    progress.step("Copying Crosspane's files…");
    let mut needs_download = false;
    let mut files_reported = false;
    match files::place(
        paths,
        inventory,
        &sources,
        route == Route::CompleteReinstall,
        &mut backup,
    ) {
        Ok(FilesOutcome::Unchanged) => setup_note!("converge: files unchanged"),
        Ok(FilesOutcome::Replaced(replaced)) => {
            setup_note!("converge: replaced {} files", replaced.len());
        }
        Ok(FilesOutcome::NeedsDownloadedInstaller(missing)) => {
            setup_note!("converge: {} files cannot be restored here", missing.len());
            needs_download = true;
            lines.push(text::NEEDS_DOWNLOADED_INSTALLER.to_owned());
        }
        // Crosspane may already be stopped, with some files replaced: each replacement is whole,
        // so every file is either its old or its new version. Report the failure and carry on, so
        // that the sign-in task check (which removes the task if the agent or the installer is no
        // longer valid), the Start menu and the restart below still run on what is in place now.
        Err(error) => {
            setup_note!("converge: placing files failed: {}", error.text);
            files_reported = true;
            lines.push(error.text);
        }
    }
    let states = files::inspect(paths, inventory);
    let file_ok = |wanted: PayloadFile| {
        states
            .iter()
            .any(|(file, state)| *file == wanted && *state == FileState::Ok)
    };
    let files_ok = !states.is_empty() && states.iter().all(|(_, state)| *state == FileState::Ok);
    if !files_ok && !needs_download && !files_reported {
        lines.push("Some of Crosspane's files are still missing or different.".to_owned());
    }

    // The sign-in task runs the supervisor (this installer), and the supervisor runs the agent;
    // neither checks a digest when it starts. A task may only stay registered while both programs
    // match their approved files, so a repair that can't restore either removes it (fail closed).
    let programs_ok = file_ok(PayloadFile::Agent) && file_ok(PayloadFile::Installer);
    let mut task_reported = false;
    match task_action(programs_ok, task::inspect(paths), route) {
        TaskAction::Remove => {
            progress.step("Removing the sign-in task until Crosspane's files are restored…");
            setup_note!("converge: agent or installer failed validation, removing the task");
            if let Err(error) = task::delete() {
                setup_note!("converge: task delete failed: {}", error.text);
                task_reported = true;
                lines.push(format!(
                    "Crosspane's sign-in task couldn't be removed. {}",
                    error.text
                ));
            }
        }
        TaskAction::Register => {
            progress.step("Registering the sign-in task…");
            if let Err(error) = task::register(paths) {
                setup_note!("converge: task register failed: {}", error.text);
                task_reported = true;
                lines.push(format!(
                    "Crosspane couldn't be set to start when you sign in. {}",
                    error.text
                ));
            }
        }
        TaskAction::Keep => {}
    }
    let task_ok = programs_ok && task::inspect(paths) == TaskHealth::Healthy;
    if !task_ok && !task_reported {
        lines.push(if programs_ok {
            "Crosspane isn't set to start when you sign in yet.".to_owned()
        } else {
            "Crosspane won't start when you sign in until its files are restored.".to_owned()
        });
    }

    progress.step("Adding Crosspane to the Start menu…");
    if let Err(error) = shell::ensure(paths, inventory.version(), estimated_kb(inventory)) {
        setup_note!("converge: shell integration failed: {}", error.text);
        lines.push(format!(
            "Crosspane couldn't be added to the Start menu and Apps & features. {}",
            error.text
        ));
    }

    // Only now: the helper, the driver files and the agent path must be the installed ones.
    let work = prepare_elevated(
        paths,
        inventory,
        options,
        payload.as_deref(),
        &plan,
        &mut lines,
    );
    // Windows can leave its approval prompt unanswered for minutes, and Crosspane is stopped until
    // it is. A step that leaves the display driver alone (the firewall rule) doesn't have to hold
    // Crosspane up, so it runs once Crosspane is back. One that installs or removes the driver
    // stays ahead of the start, because the agent must not start under a driver that is changing.
    let (now, later) = if defers_until_started(&work) {
        (None, Some(work))
    } else {
        (Some(work), None)
    };
    let mut elevated = match now {
        Some(work) => run_elevated(paths, work, progress, &mut lines),
        None => ElevatedOutcome::NotNeeded,
    };

    // Crosspane is started whenever the sign-in task is right and the two programs that run (the
    // agent and the supervisor, which is this installer) match their approved files. A damaged
    // file that can't be restored here (the interface, say) must not leave Crosspane stopped.
    let start_wanted = task_ok && file_ok(PayloadFile::Agent) && file_ok(PayloadFile::Installer);
    if files_ok && task_ok {
        write_state(paths, inventory, &states, &mut lines);
    }
    legacy::prune(paths, BACKUPS_KEPT);

    // Everything that changes files is done. Starting changes none, so the lock is released here
    // (on the thread that took it): the supervisor sees the lock as a setup replacing files and
    // would give up on an agent whose first start fails, while this waits for it.
    drop(lock);
    let mut agent_answering = false;
    if start_wanted {
        agent_answering = start_inner(paths, progress);
        if !agent_answering {
            lines.push(text::NOT_STARTED_YET.to_owned());
        }
    }
    // The firewall step changes no installed file, and the kit it runs from is held while it
    // runs, so it needs no lock either.
    if let Some(work) = later {
        elevated = run_elevated(paths, work, progress, &mut lines);
    }

    let mut report = ConvergeReport {
        files_ok,
        task_ok,
        agent_answering,
        elevated,
        lines,
    };
    if report.ok() {
        report.lines.insert(0, success_line(route).to_owned());
    }
    setup_note!(
        "converge: done files_ok={} task_ok={} agent_answering={} administrator_step={}",
        report.files_ok,
        report.task_ok,
        report.agent_answering,
        outcome_name(&report.elevated)
    );
    Ok(report)
}

/// Whether an install is refused before it changes anything. Files that are missing or damaged
/// can't be put back without the downloaded files, so an install that would leave some of them
/// behind stops here instead of replacing only the rest. A repair and a complete reinstall carry
/// on: they restore what they can and report the rest.
fn refuses_install(route: Route, plan: &ConvergePlan) -> bool {
    route == Route::Install && !plan.unrestorable.is_empty()
}

fn success_line(route: Route) -> &'static str {
    match route {
        Route::Install => "Crosspane is installed and running.",
        Route::Repair => "Crosspane is repaired and running.",
        Route::CompleteReinstall => "Crosspane was reinstalled and is running.",
    }
}

/// The installed size in KB, for Apps & features.
fn estimated_kb(inventory: &Inventory) -> u32 {
    let bytes: u64 = PayloadFile::ALL
        .iter()
        .map(|file| inventory.pin(*file).size)
        .sum();
    u32::try_from(bytes / 1024).unwrap_or(u32::MAX)
}

/// `installed.json`: informational only, written after the files and the task are settled. A
/// failure is a line, never a stop.
fn write_state(
    paths: &Paths,
    inventory: &Inventory,
    states: &[(PayloadFile, FileState)],
    lines: &mut Vec<String>,
) {
    let files = states
        .iter()
        .map(|(file, _)| {
            (
                file.relative().to_owned(),
                hex(&inventory.pin(*file).sha256),
            )
        })
        .collect();
    let installed = Installed {
        schema: 1,
        version: inventory.version().to_owned(),
        installed_at_unix_ms: unix_ms(),
        files,
    };
    if let Err(error) = state::write(paths, &installed) {
        setup_note!("converge: could not record the install: {}", error.text);
        lines.push(format!(
            "Crosspane is installed, but setup couldn't save a note about it. {}",
            error.text
        ));
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// What the administrator step is to do, decided from the helper's status but not yet run.
#[derive(Debug)]
enum ElevatedWork<'a> {
    /// Nothing to run: this is the step's outcome (not needed, not allowed, or not checkable).
    Done(ElevatedOutcome),
    /// Run `plan` from the helper files `kit` pins.
    Run {
        plan: ElevatedPlan,
        kit: &'a KitManifest,
    },
}

/// Whether the step waits until Crosspane has started: it has something to run, and that leaves
/// the display driver alone.
fn defers_until_started(work: &ElevatedWork<'_>) -> bool {
    matches!(work, ElevatedWork::Run { plan, .. } if !touches_driver(plan.verb()))
}

/// The administrator step, per policy: reads the helper's status and settles what to run. Nothing
/// elevated runs here. Never fatal: a step that can't go ahead is an outcome for the report.
fn prepare_elevated<'a>(
    paths: &Paths,
    inventory: &'a Inventory,
    options: &ConvergeOptions,
    payload: Option<&Path>,
    plan: &ConvergePlan,
    lines: &mut Vec<String>,
) -> ElevatedWork<'a> {
    let consented = match &options.elevated {
        ElevatedPolicy::Never => {
            if let Some(note) = &plan.elevated_note {
                lines.push(note.clone());
            }
            return ElevatedWork::Done(ElevatedOutcome::NotNeeded);
        }
        ElevatedPolicy::IfNeeded => None,
        ElevatedPolicy::Consented(consented) => Some(consented),
    };
    let known_scope = consented.and_then(|plan| plan.verb().scope().cloned());
    let wanted = match observe_verb(paths, payload, known_scope, elevated::setup_verb) {
        Ok(Some(wanted)) => wanted,
        Ok(None) => return ElevatedWork::Done(ElevatedOutcome::NotNeeded),
        Err(reason) => {
            setup_note!("converge: administrator step not checked: {reason}");
            lines.push(reason.clone());
            return ElevatedWork::Done(ElevatedOutcome::Failed(reason));
        }
    };
    let to_run = match consented {
        // The person agreed to these lines. If the state moved on so that none of them is called
        // for any more, there is nothing left to approve.
        Some(consented) if !elevated::shares_a_part(consented.verb(), &wanted) => {
            return ElevatedWork::Done(ElevatedOutcome::NotNeeded);
        }
        Some(consented) => consented.clone(),
        None => match elevated::plan(wanted) {
            Ok(planned) => planned,
            Err(error) => {
                lines.push(error.text.clone());
                return ElevatedWork::Done(ElevatedOutcome::Failed(error.text));
            }
        },
    };
    let Some(kit) = inventory.kit() else {
        let reason = "This installer can't set up the firewall rule and display driver.";
        lines.push(reason.to_owned());
        return ElevatedWork::Done(ElevatedOutcome::Failed(reason.to_owned()));
    };
    ElevatedWork::Run { plan: to_run, kit }
}

/// Runs the prepared administrator step and records what happened. Never fatal: its outcome goes
/// in the report.
fn run_elevated(
    paths: &Paths,
    work: ElevatedWork<'_>,
    progress: &dyn Progress,
    lines: &mut Vec<String>,
) -> ElevatedOutcome {
    let (plan, kit) = match work {
        ElevatedWork::Done(outcome) => return outcome,
        ElevatedWork::Run { plan, kit } => (plan, kit),
    };
    // `elevated::run` tells the person it is waiting for approval, and keeps saying so.
    let outcome = elevated::run(paths, kit, &plan, progress);
    setup_note!("converge: administrator step {}", outcome_name(&outcome));
    match &outcome {
        ElevatedOutcome::NotNeeded => {}
        ElevatedOutcome::Configured(summary) => lines.push(summary.clone()),
        ElevatedOutcome::Declined => lines.push(declined_line(plan.verb())),
        ElevatedOutcome::Failed(reason) => lines.push(reason.clone()),
    }
    outcome
}

/// The line for a declined approval prompt: what that leaves undone, for what was asked.
fn declined_line(verb: &Verb) -> String {
    format!(
        "Administrator approval was declined. {}",
        elevated::without_setup_note(verb)
    )
}

/// The outcome's variant name only: the texts inside may be shown to the person, not logged.
fn outcome_name(outcome: &ElevatedOutcome) -> &'static str {
    match outcome {
        ElevatedOutcome::NotNeeded => "not needed",
        ElevatedOutcome::Configured(_) => "configured",
        ElevatedOutcome::Declined => "declined",
        ElevatedOutcome::Failed(_) => "failed",
    }
}

/// Refuses while another setup is working; then `start_inner`: if `!supervisor::running()`:
/// `task::run` (fallback spawn); wait ≤45 s. The lock is held only for the check, never for the
/// start or the wait: starting changes no files, and the supervisor gives up on a failing agent
/// while the lock exists.
pub(crate) fn start(paths: &Paths, progress: &dyn Progress) -> SetupResult<bool> {
    let lock = OperationLock::acquire()?;
    let installed = paths.file(PayloadFile::Agent).is_file();
    drop(lock);
    if !installed {
        return Err(SetupError::before(text::NOT_INSTALLED));
    }
    Ok(start_inner(paths, progress))
}

/// Starts the sign-in supervisor, then waits for the agent to answer. A supervisor that already
/// runs is left alone when the agent answers within `HUNG_PROBE`; otherwise the agent is hung, and
/// everything is stopped (under the operation lock) and started again. While another setup holds
/// the operation lock nothing is launched from here: that setup starts Crosspane itself, and this
/// only waits for the agent.
///
/// The caller must not hold the operation lock: the supervisor treats it as a setup replacing
/// files and gives up on an agent that fails, so it is taken here only around the stop.
fn start_inner(paths: &Paths, progress: &dyn Progress) -> bool {
    if supervisor::running() {
        setup_note!("start: the supervisor is already running");
        progress.step("Checking that Crosspane answers…");
        if agent_pipe::wait_until_answering(paths, HUNG_PROBE).is_some() {
            return true;
        }
        setup_note!("start: the agent doesn't answer, so it is stopped and started again");
        match OperationLock::acquire() {
            Ok(lock) => {
                let stopped = stop::stop_all(paths, progress);
                drop(lock);
                setup_note!(
                    "start: stop graceful={} remaining={}",
                    stopped.graceful,
                    stopped.remaining.len()
                );
            }
            Err(error) => {
                // Another setup is working on Crosspane and starts it itself when it is done.
                setup_note!("start: not restarting the agent: {}", error.text);
                return wait_for_agent(paths, progress);
            }
        }
    }
    // The lock was dropped before this call, so another setup may have taken it since. It starts
    // Crosspane itself when it is done; launching the supervisor now would race it.
    if lock::mutex_exists(lock::OPERATION_MUTEX) {
        setup_note!("start: another setup is working, so the supervisor isn't started from here");
        return wait_for_agent(paths, progress);
    }
    progress.step("Starting Crosspane…");
    launch_supervisor(paths);
    wait_for_agent(paths, progress)
}

/// Runs the sign-in task; if the supervisor doesn't appear, starts it directly.
fn launch_supervisor(paths: &Paths) {
    // A task instance left behind would make `IgnoreNew` swallow the Run below.
    if let Err(error) = task::stop_instances() {
        setup_note!("start: could not end the old task instance: {}", error.text);
    }
    let requested = match task::run() {
        Ok(()) => true,
        Err(error) => {
            setup_note!("start: task run failed: {}", error.text);
            false
        }
    };
    if requested {
        let deadline = Instant::now() + SUPERVISOR_WAIT;
        while !supervisor::running() && Instant::now() < deadline {
            thread::sleep(POLL);
        }
    }
    if supervisor::running() {
        setup_note!("start: the sign-in task started the supervisor");
    } else {
        spawn_supervisor(paths);
    }
}

/// Starts the installed `crosspane-installer.exe --windows-supervisor` detached from this process.
/// A job that forbids breakaway gets a second try without it.
fn spawn_supervisor(paths: &Paths) {
    let program = paths.file(PayloadFile::Installer);
    let detached = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    let attempts = [
        (detached | CREATE_BREAKAWAY_FROM_JOB, "with breakaway"),
        (detached, "without breakaway"),
    ];
    for (flags, how) in attempts {
        let spawned = Command::new(&program)
            .arg(supervisor::SUPERVISOR_FLAG)
            .current_dir(&paths.install)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn();
        match spawned {
            Ok(child) => {
                // The supervisor outlives this call; dropping the handle doesn't stop it.
                drop(child);
                setup_note!("start: started the supervisor directly ({how})");
                return;
            }
            Err(error)
                if error.raw_os_error() == i32::try_from(ERROR_ACCESS_DENIED).ok()
                    && flags & CREATE_BREAKAWAY_FROM_JOB != 0 =>
            {
                note_io(
                    "start: the supervisor couldn't break away from its job",
                    &error,
                );
            }
            Err(error) => {
                note_io("start: the supervisor couldn't be started", &error);
                return;
            }
        }
    }
}

/// Waits up to 45 s for the agent to answer, with a progress line at least every 15 s.
fn wait_for_agent(paths: &Paths, progress: &dyn Progress) -> bool {
    progress.step("Waiting for Crosspane to start…");
    let deadline = Instant::now() + AGENT_WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        if agent_pipe::wait_until_answering(paths, remaining.min(HEARTBEAT)).is_some() {
            return true;
        }
        progress.step("Still waiting for Crosspane to start…");
    }
}

#[derive(Clone, Debug)]
pub(crate) struct UninstallOptions {
    pub remove_settings: bool,
    pub teardown: ElevatedPolicy,
}

#[derive(Clone, Debug)]
pub(crate) struct UninstallReport {
    pub left: Vec<String>,
    pub lines: Vec<String>,
}

/// The preview of an uninstall, and the administrator plan when `teardown` is wanted and there is
/// a firewall rule or driver to remove.
pub(crate) fn plan_uninstall(
    paths: &Paths,
    remove_settings: bool,
    teardown: bool,
) -> SetupResult<(String, Option<ElevatedPlan>)> {
    let mut lines = uninstall_preview(remove_settings);
    let mut plan = None;
    if teardown {
        // The installed helper, because the files are still in place.
        match observe_verb(paths, None, None, elevated::teardown_verb) {
            Ok(Some(verb)) => {
                let planned = elevated::plan(verb)?;
                lines.push(planned.preview_block());
                plan = Some(planned);
            }
            Ok(None) => {}
            Err(reason) => {
                setup_note!("uninstall plan: firewall rule and driver not checked: {reason}");
                lines.push(ADMIN_NOT_CHECKED.to_owned());
            }
        }
    }
    Ok((lines.join("\n"), plan))
}

fn uninstall_preview(remove_settings: bool) -> Vec<String> {
    vec![
        "Stop Crosspane.".to_owned(),
        "Remove the sign-in task.".to_owned(),
        "Remove Crosspane from the Start menu and Apps & features.".to_owned(),
        "Delete Crosspane's program files.".to_owned(),
        if remove_settings {
            "Delete your settings and pairings."
        } else {
            "Keep your settings and pairings."
        }
        .to_owned(),
    ]
}

const LEFT_PROGRAMS: &str = "program files";
const LEFT_SETTINGS: &str = "settings and pairings";
const LEFT_TASK: &str = "sign-in task";
const KEY_NOT_ERASED: &str = "Your settings were removed but the sign-in key couldn't be erased.";
const ADMIN_NOT_CHECKED: &str =
    "The firewall rule and display driver can't be checked right now, so they stay as they are.";

/// leave the install folder → lock → stop → `task::delete` → [teardown] → [erase-identity if
/// `remove_settings`] → `shell::remove` → remove the install dir (move our own exe to backups or
/// temp first) → [remove the state and config dirs].
pub(crate) fn uninstall(
    paths: &Paths,
    inventory: &Inventory,
    options: &UninstallOptions,
    progress: &dyn Progress,
) -> SetupResult<UninstallReport> {
    leave_install_dir(paths);
    let _lock = OperationLock::acquire()?;
    let mut left: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    let stopped = stop::stop_all(paths, progress);
    if !stopped.remaining.is_empty() {
        setup_note!(
            "uninstall: {} programs still running",
            stopped.remaining.len()
        );
        left.push(format!(
            "programs that are still running ({})",
            stopped.remaining.join(", ")
        ));
    }

    progress.step("Removing the sign-in task…");
    if let Err(error) = task::delete() {
        setup_note!("uninstall: task delete failed: {}", error.text);
        left.push(LEFT_TASK.to_owned());
    }

    teardown(
        paths,
        inventory,
        &options.teardown,
        progress,
        &mut left,
        &mut notes,
    );

    if options.remove_settings {
        progress.step("Erasing the sign-in key…");
        if !erase_identity(paths) {
            notes.push(KEY_NOT_ERASED.to_owned());
        }
    }

    progress.step("Removing Crosspane's files…");
    left.extend(shell::remove(paths));
    remove_programs(paths, options.remove_settings, &mut left);

    match fs::remove_file(paths.installed_json()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => note_io("uninstall: couldn't remove the install note", &error),
    }
    // Only succeeds when empty, which is the point: it is setup's own folder.
    if fs::remove_dir(&paths.installer_state).is_err() {
        setup_note!("uninstall: setup's own folder is kept");
    }

    if options.remove_settings {
        for dir in [&paths.state, &paths.config] {
            if !(is_crosspane_dir(dir) && remove_retrying(dir))
                && !left.iter().any(|name| name == LEFT_SETTINGS)
            {
                left.push(LEFT_SETTINGS.to_owned());
            }
        }
    } else {
        legacy::prune(paths, BACKUPS_KEPT);
    }

    let mut lines = uninstall_lines(&left);
    lines.extend(notes);
    setup_note!("uninstall: done, {} parts left", left.len());
    Ok(UninstallReport { left, lines })
}

/// The one line an uninstall that left nothing behind starts its report with.
const UNINSTALLED: &str = "Crosspane was uninstalled.";

/// Whether `line` is the summary an operation starts its report with, once it fully worked: the
/// success line of a converge route, or the uninstall's. A screen that prints its own headline
/// leaves it out. The headless command shows it.
pub(crate) fn is_summary_line(line: &str) -> bool {
    line == UNINSTALLED
        || [Route::Install, Route::Repair, Route::CompleteReinstall]
            .into_iter()
            .any(|route| line == success_line(route))
}

/// The summary lines of an uninstall: done, or what is still there with a retry.
fn uninstall_lines(left: &[String]) -> Vec<String> {
    if left.is_empty() {
        vec![UNINSTALLED.to_owned()]
    } else {
        vec![
            format!("Some parts are still there: {}.", left.join(", ")),
            "Try again.".to_owned(),
        ]
    }
}

/// The optional firewall rule and display driver removal. It must run while the installed helper
/// is still in place.
fn teardown(
    paths: &Paths,
    inventory: &Inventory,
    policy: &ElevatedPolicy,
    progress: &dyn Progress,
    left: &mut Vec<String>,
    notes: &mut Vec<String>,
) {
    let consented = match policy {
        ElevatedPolicy::Never => return,
        ElevatedPolicy::IfNeeded => None,
        ElevatedPolicy::Consented(consented) => Some(consented),
    };
    let known_scope = consented.and_then(|plan| plan.verb().scope().cloned());
    let wanted = match observe_verb(paths, None, known_scope, elevated::teardown_verb) {
        Ok(Some(wanted)) => wanted,
        Ok(None) => return,
        Err(reason) => {
            // Not "left": nothing a retry could fix, and it must never keep an uninstall open.
            setup_note!("uninstall: firewall rule and driver not checked: {reason}");
            notes.push(ADMIN_NOT_CHECKED.to_owned());
            return;
        }
    };
    let to_run = match consented {
        // The person agreed to these lines. If none of them is called for any more, there is
        // nothing left to approve.
        Some(consented) if !elevated::shares_a_part(consented.verb(), &wanted) => return,
        Some(consented) => consented.clone(),
        None => {
            let name = admin_parts(&wanted);
            match elevated::plan(wanted) {
                Ok(planned) => planned,
                Err(error) => {
                    setup_note!("uninstall: teardown plan failed: {}", error.text);
                    left.push(name.to_owned());
                    return;
                }
            }
        }
    };
    let name = admin_parts(to_run.verb());
    let Some(kit) = inventory.kit() else {
        left.push(name.to_owned());
        return;
    };
    progress.step(&format!("Removing the {name}…"));
    let outcome = elevated::run(paths, kit, &to_run, progress);
    setup_note!("uninstall: administrator step {}", outcome_name(&outcome));
    match outcome {
        ElevatedOutcome::NotNeeded | ElevatedOutcome::Configured(_) => {}
        ElevatedOutcome::Declined => {
            left.push(name.to_owned());
            notes.push(kept_note(to_run.verb()));
        }
        ElevatedOutcome::Failed(reason) => {
            left.push(name.to_owned());
            notes.push(reason);
        }
    }
}

/// What an administrator step covers, in words: the firewall rule, the display driver, or both.
fn admin_parts(verb: &Verb) -> &'static str {
    match verb {
        Verb::AddFirewall(_) | Verb::RemoveFirewall(_) => "firewall rule",
        Verb::InstallDriver | Verb::RemoveDriver => "display driver",
        _ => "firewall rule and display driver",
    }
}

/// The note for a declined removal prompt: what was kept, for what was asked.
fn kept_note(verb: &Verb) -> String {
    let (name, were) = match verb {
        Verb::Teardown(_) | Verb::Setup(_) => ("firewall rule and display driver", "were"),
        other => (admin_parts(other), "was"),
    };
    format!("Administrator approval was declined, so the {name} {were} kept.")
}

/// Runs the installed agent's `erase-identity`, hidden, for up to 30 s. True when the sign-in key
/// is gone (or there was nothing to erase). The command exits 0 even when it refuses, so the
/// receipt it prints decides.
fn erase_identity(paths: &Paths) -> bool {
    let agent = paths.file(PayloadFile::Agent);
    if !agent.is_file() {
        // Without the agent nothing can be erased; there is only something to lose if it ever ran.
        return !paths.state.exists();
    }
    let mut child = match Command::new(&agent)
        .arg("erase-identity")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            note_io("uninstall: the identity erase couldn't start", &error);
            return false;
        }
    };
    let reader = child.stdout.take().map(|out| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            // A receipt is one short line; anything longer is not read.
            let _ = out.take(8 * 1024).read_to_end(&mut bytes);
            bytes
        })
    });
    let deadline = Instant::now() + ERASE_WAIT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                setup_note!("uninstall: the identity erase took too long");
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(error) => {
                note_io("uninstall: the identity erase can't be watched", &error);
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    let erased = status.is_some_and(|status| status.success()) && receipt_confirms(&output);
    setup_note!("uninstall: identity erased={erased}");
    erased
}

/// Whether the agent's erase receipt (the last JSON object it printed) reports the key gone.
fn receipt_confirms(output: &[u8]) -> bool {
    String::from_utf8_lossy(output)
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .find(serde_json::Value::is_object)
        .and_then(|receipt| {
            receipt
                .get("result")
                .and_then(serde_json::Value::as_str)
                .map(|result| matches!(result, "removed" | "already_absent"))
        })
        .unwrap_or(false)
}

/// Removes the install directory after setting aside our own running exe and any leftovers of an
/// older setup. Anything that can't go is added to `left`.
fn remove_programs(paths: &Paths, remove_settings: bool, left: &mut Vec<String>) {
    let mut backup = Backup::new(paths);
    if let Ok(exe) = std::env::current_exe()
        && paths::is_under(&exe, &paths.install)
    {
        // A running exe can be renamed on the same volume but not deleted.
        let moved = if remove_settings {
            // The state folder (and its backups) is going away too, so use the temp folder.
            fs::create_dir_all(&paths.temp).and_then(|()| {
                fs::rename(
                    &exe,
                    paths
                        .temp
                        .join(format!("crosspane-installer-{}.exe", legacy::utc_stamp())),
                )
            })
        } else {
            backup.stash(&exe, "uninstalled").map(|_| ())
        };
        if let Err(error) = moved {
            note_io(
                "uninstall: couldn't move the running installer aside",
                &error,
            );
        }
    }

    if remove_settings {
        // The install and state folders are deleted whole; only what sits outside them (the older
        // setup's quarantine folders) needs deleting on its own.
        for item in legacy::items(paths) {
            if !paths::is_under(&item, &paths.install) && !paths::is_under(&item, &paths.state) {
                remove_retrying(&item);
            }
        }
    } else {
        // Set aside, never deleted unseen. What can't be moved is deleted, and checked below.
        let stuck = legacy::clean(paths, &mut backup);
        if !stuck.is_empty() {
            setup_note!("uninstall: {} old items could not be moved", stuck.len());
        }
    }

    if !(is_crosspane_dir(&paths.install) && remove_retrying(&paths.install)) {
        left.push(LEFT_PROGRAMS.to_owned());
    }

    // Anything of the older setup still there, apart from what the program folder and the
    // settings folder (deleted by the caller when asked) are reported for.
    let stuck = legacy::items(paths).into_iter().any(|item| {
        !paths::is_under(&item, &paths.install)
            && !(remove_settings && paths::is_under(&item, &paths.state))
    });
    if stuck {
        left.push("files left by an older setup".to_owned());
    }
}

/// Setup only ever deletes folders named `Crosspane`; anything else is left alone.
fn is_crosspane_dir(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("Crosspane"))
}

/// Deletes a file or folder tree, retrying for up to 5 s while another program holds something.
/// True when it is gone.
fn remove_retrying(path: &Path) -> bool {
    let deadline = Instant::now() + REMOVE_WAIT;
    loop {
        let result = match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_dir() => fs::remove_dir_all(path),
            Ok(_) => fs::remove_file(path),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => return true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(error) => {
                let busy = error
                    .raw_os_error()
                    .is_some_and(|code| BUSY_CODES.contains(&code));
                if !busy || Instant::now() >= deadline {
                    note_io("uninstall: couldn't delete an item", &error);
                    return !path.exists();
                }
                thread::sleep(POLL);
            }
        }
    }
}

/// Whether Crosspane is installed for this account: the agent, the install note or the task.
pub(crate) fn is_installed(paths: &Paths) -> bool {
    paths.file(PayloadFile::Agent).exists()
        || paths.installed_json().exists()
        || task::inspect(paths) != TaskHealth::Missing
}

/// The user-facing texts. Plain words only: never ownership or bookkeeping vocabulary.
pub(crate) mod text {
    pub(crate) const NEEDS_DOWNLOADED_INSTALLER: &str =
        "Run the Crosspane installer you downloaded to restore Crosspane's files.";
    pub(crate) const NOT_STARTED_YET: &str =
        "Crosspane is installed but hasn't started yet. Check again.";
    pub(crate) const BUSY: &str =
        "Another Crosspane Setup is working right now. Try again when it finishes.";
    pub(crate) const STILL_RUNNING: &str = "Crosspane couldn't be stopped. Close it and try again.";
    pub(crate) const SOURCES_DAMAGED: &str =
        "The Crosspane files beside this installer don't match it. Download Crosspane again.";
    pub(crate) const NO_INVENTORY: &str =
        "This installer build doesn't list Crosspane's files, so it can't install them.";
    pub(crate) const NOT_INSTALLED: &str = "Crosspane isn't installed for this account.";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_installer_core::elevated::{
        AgentProgram, InstallId,
        kit::{KitDocument, KitDocumentFile, KitFile},
        step::{INSTALL_WITHOUT_SETUP, PREVIEW_HEADER},
    };

    #[test]
    fn a_task_never_stays_registered_for_programs_that_failed_validation() {
        let healths = [
            TaskHealth::Healthy,
            TaskHealth::Missing,
            TaskHealth::Disabled,
            TaskHealth::Different,
            TaskHealth::Unavailable,
        ];
        let routes = [Route::Install, Route::Repair, Route::CompleteReinstall];
        for health in healths {
            for route in routes {
                assert_eq!(
                    task_action(false, health, route),
                    TaskAction::Remove,
                    "{health:?} {route:?}"
                );
            }
        }
        assert_eq!(
            task_action(true, TaskHealth::Healthy, Route::Install),
            TaskAction::Keep
        );
        assert_eq!(
            task_action(true, TaskHealth::Healthy, Route::Repair),
            TaskAction::Register
        );
        assert_eq!(
            task_action(true, TaskHealth::Disabled, Route::Install),
            TaskAction::Register
        );
    }

    const BANNED: [&str; 7] = [
        "foreign",
        "unsettled",
        "admission",
        "submission",
        "epoch",
        "journal",
        "outcomeunknown",
    ];

    fn rule_scope() -> RuleScope {
        RuleScope {
            id: InstallId::parse("0123456789abcdef").unwrap(),
            program: AgentProgram::parse(
                r"C:\Users\someone\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
            )
            .unwrap(),
        }
    }

    fn setup_plan() -> ElevatedPlan {
        ElevatedPlan::new(Verb::Setup(rule_scope())).unwrap()
    }

    fn test_kit() -> KitManifest {
        let entry = |file| KitDocumentFile {
            file,
            size: 1,
            sha256: "11".repeat(32),
        };
        KitManifest::admit(KitDocument {
            schema_version: 1,
            files: KitFile::ALL.into_iter().map(entry).collect(),
        })
        .unwrap()
    }

    fn plain_plan() -> ConvergePlan {
        ConvergePlan {
            copies: Vec::new(),
            unrestorable: Vec::new(),
            restart: false,
            legacy: false,
            task: false,
            shell: false,
            start: true,
            elevated: None,
            elevated_note: None,
        }
    }

    fn full_plan(elevated: Option<ElevatedPlan>) -> ConvergePlan {
        ConvergePlan {
            copies: vec![PayloadFile::Agent, PayloadFile::Ui, PayloadFile::Ctl],
            unrestorable: vec![PayloadFile::Helper],
            restart: true,
            legacy: true,
            task: true,
            shell: true,
            start: true,
            elevated,
            elevated_note: Some(APPROVAL_PENDING.to_owned()),
        }
    }

    fn has_header(preview: &str) -> bool {
        preview.lines().any(|line| line.starts_with(PREVIEW_HEADER))
    }

    const ROUTES: [Route; 3] = [Route::Install, Route::Repair, Route::CompleteReinstall];

    fn firewall_plan() -> ElevatedPlan {
        ElevatedPlan::new(Verb::AddFirewall(rule_scope())).unwrap()
    }

    fn driver_plan() -> ElevatedPlan {
        ElevatedPlan::new(Verb::InstallDriver).unwrap()
    }

    #[test]
    fn the_preview_carries_the_approval_header_only_with_an_administrator_plan() {
        for route in ROUTES {
            assert!(!has_header(&plain_plan().preview_with(route, true)));
            assert!(!has_header(&full_plan(None).preview_with(route, true)));
            let with = full_plan(Some(setup_plan())).preview_with(route, true);
            assert!(has_header(&with), "{route:?}");
            assert!(with.contains(INSTALL_DECLINE_NOTE), "{route:?}");
        }
        let without = full_plan(None).preview_with(Route::Install, true);
        assert!(!without.contains(INSTALL_DECLINE_NOTE));
    }

    #[test]
    fn the_preview_stays_in_plain_words() {
        for route in ROUTES {
            for plan in [
                plain_plan(),
                full_plan(None),
                full_plan(Some(setup_plan())),
                full_plan(Some(firewall_plan())),
                full_plan(Some(driver_plan())),
            ] {
                for running in [false, true] {
                    let preview = plan.preview_with(route, running).to_lowercase();
                    for word in BANNED {
                        assert!(!preview.contains(word), "{route:?} mentions {word}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_preview_fits_the_controller_limit() {
        for route in ROUTES {
            let preview = full_plan(Some(setup_plan())).preview_with(route, true);
            assert!(preview.len() <= 2_400, "{} bytes", preview.len());
        }
    }

    #[test]
    fn an_install_that_cannot_restore_its_files_is_refused() {
        let missing = ConvergePlan {
            unrestorable: vec![PayloadFile::Helper],
            ..plain_plan()
        };
        // Stopping Crosspane for an install that can't finish would leave it stopped.
        assert!(refuses_install(Route::Install, &missing));
        assert!(!refuses_install(Route::Install, &plain_plan()));
        // The other routes restore what they can and report the rest.
        for route in [Route::Repair, Route::CompleteReinstall] {
            assert!(!refuses_install(route, &missing), "{route:?}");
            assert!(!refuses_install(route, &plain_plan()), "{route:?}");
        }
        // Files that can be copied don't count.
        let copyable = ConvergePlan {
            copies: vec![PayloadFile::Agent],
            ..plain_plan()
        };
        assert!(!refuses_install(Route::Install, &copyable));
    }

    #[test]
    fn the_stop_line_appears_only_when_something_is_running() {
        let stops = |plan: &ConvergePlan, running: bool| {
            plan.preview_with(Route::Install, running)
                .lines()
                .find(|line| line.starts_with("Stop Crosspane"))
                .map(str::to_owned)
        };
        let replacing = ConvergePlan {
            restart: true,
            copies: vec![PayloadFile::Agent],
            ..plain_plan()
        };
        assert_eq!(
            stops(&replacing, true).as_deref(),
            Some("Stop Crosspane while its files are replaced.")
        );
        // Nothing running, as right after an uninstall: nothing to stop.
        assert_eq!(stops(&replacing, false), None);
        let brief = ConvergePlan {
            restart: true,
            ..plain_plan()
        };
        assert_eq!(
            stops(&brief, true).as_deref(),
            Some("Stop Crosspane for a moment.")
        );
        assert_eq!(stops(&brief, false), None);
        // A plan that doesn't stop Crosspane never says so.
        assert_eq!(stops(&plain_plan(), true), None);
        // The rest of the preview doesn't depend on it.
        let without = replacing.preview_with(Route::Install, false);
        assert!(without.lines().any(|line| line == "Replace 1 file."));
        assert!(without.lines().any(|line| line == "Start Crosspane."));
    }

    #[test]
    fn the_decline_sentence_follows_the_administrator_step() {
        let preview = |plan: ElevatedPlan| full_plan(Some(plan)).preview_with(Route::Install, true);
        let firewall = preview(firewall_plan());
        let driver = preview(driver_plan());
        let both = preview(setup_plan());
        // The approval block stays whatever the step: consent depends on it.
        for shown in [&firewall, &driver, &both] {
            assert!(has_header(shown), "{shown}");
        }
        assert!(
            firewall
                .lines()
                .any(|line| line == "If you decline, Crosspane is still installed, but other computers may not reach this PC until the firewall rule is added.")
        );
        assert!(
            driver
                .lines()
                .any(|line| line == "If you decline, Crosspane is still installed and mirrors windows in place without the display driver.")
        );
        assert!(both.lines().any(|line| line == INSTALL_DECLINE_NOTE));
        // The core "them" only fits a step that asked for both.
        assert!(!firewall.contains(INSTALL_DECLINE_NOTE));
        assert!(!driver.contains(INSTALL_DECLINE_NOTE));
        // Exactly one decline sentence in each.
        for shown in [&firewall, &driver, &both] {
            assert_eq!(
                shown
                    .lines()
                    .filter(|line| line.starts_with("If you decline"))
                    .count(),
                1,
                "{shown}"
            );
        }
        // Without a step there is no sentence.
        let none = full_plan(None).preview_with(Route::Install, true);
        assert!(!none.contains("If you decline"));
    }

    #[test]
    fn the_preview_lists_what_will_happen() {
        let preview = full_plan(None).preview_with(Route::Install, true);
        for expected in [
            "Install Crosspane for this account.",
            "Stop Crosspane while its files are replaced.",
            "Replace 3 files.",
            "Clean up files left by an older setup.",
            "Register Crosspane to start when you sign in.",
            "Add Crosspane to the Start menu and Apps & features.",
            "Start Crosspane.",
            text::NEEDS_DOWNLOADED_INSTALLER,
            APPROVAL_PENDING,
        ] {
            assert!(preview.lines().any(|line| line == expected), "{expected}");
        }
        assert_eq!(
            preview.lines().last(),
            Some(APPROVAL_PENDING),
            "the note comes last"
        );
    }

    #[test]
    fn the_preview_leaves_out_what_will_not_happen() {
        let preview = plain_plan().preview(Route::Install);
        assert_eq!(
            preview,
            "Install Crosspane for this account.\nStart Crosspane."
        );
        assert!(!preview.contains("Stop Crosspane"));
        assert!(!preview.contains("Replace"));
        assert!(
            ConvergePlan {
                start: false,
                ..plain_plan()
            }
            .preview(Route::Repair)
            .lines()
            .eq(["Repair Crosspane."])
        );
    }

    #[test]
    fn the_preview_names_the_route() {
        assert!(
            plain_plan()
                .preview(Route::Repair)
                .starts_with("Repair Crosspane.")
        );
        assert!(
            plain_plan()
                .preview(Route::CompleteReinstall)
                .starts_with("Reinstall all of Crosspane's files.")
        );
        let one = ConvergePlan {
            copies: vec![PayloadFile::Agent],
            ..plain_plan()
        };
        assert!(one.preview(Route::Install).contains("Replace 1 file."));
        assert!(
            one.preview(Route::Repair)
                .contains("Restore missing or changed files.")
        );
    }

    #[test]
    fn the_report_is_ok_only_when_everything_is() {
        for bits in 0..8u8 {
            let files_ok = bits & 1 != 0;
            let task_ok = bits & 2 != 0;
            let agent_answering = bits & 4 != 0;
            let report = ConvergeReport {
                files_ok,
                task_ok,
                agent_answering,
                elevated: ElevatedOutcome::Declined,
                lines: Vec::new(),
            };
            assert_eq!(
                report.ok(),
                files_ok && task_ok && agent_answering,
                "{bits:03b}"
            );
        }
    }

    #[test]
    fn the_administrator_step_never_decides_the_report() {
        for elevated in [
            ElevatedOutcome::NotNeeded,
            ElevatedOutcome::Configured("done".to_owned()),
            ElevatedOutcome::Declined,
            ElevatedOutcome::Failed("no".to_owned()),
        ] {
            let report = ConvergeReport {
                files_ok: true,
                task_ok: true,
                agent_answering: true,
                elevated,
                lines: Vec::new(),
            };
            assert!(report.ok());
        }
    }

    #[test]
    fn the_success_line_names_the_route() {
        assert_eq!(
            success_line(Route::Install),
            "Crosspane is installed and running."
        );
        assert_eq!(
            success_line(Route::Repair),
            "Crosspane is repaired and running."
        );
        assert_eq!(
            success_line(Route::CompleteReinstall),
            "Crosspane was reinstalled and is running."
        );
    }

    #[test]
    fn summary_lines_are_the_ones_the_engine_writes() {
        for route in ROUTES {
            assert!(is_summary_line(success_line(route)), "{route:?}");
        }
        assert!(is_summary_line(UNINSTALLED));
        assert_eq!(uninstall_lines(&[]), vec![UNINSTALLED.to_owned()]);
        for line in [
            "",
            "Crosspane is installed.",
            "Crosspane was uninstalled. Try again.",
            text::NOT_STARTED_YET,
            "Some parts are still there: sign-in task.",
        ] {
            assert!(!is_summary_line(line), "{line:?}");
        }
    }

    #[test]
    fn only_a_step_that_leaves_the_driver_alone_waits_for_the_start() {
        let scope = rule_scope();
        let kit = test_kit();
        let run = |verb: Verb| ElevatedWork::Run {
            plan: ElevatedPlan::new(verb).unwrap(),
            kit: &kit,
        };
        // The firewall rule alone: Crosspane doesn't stay down while the prompt waits.
        assert!(defers_until_started(&run(Verb::AddFirewall(scope.clone()))));
        assert!(defers_until_started(&run(Verb::RemoveFirewall(
            scope.clone()
        ))));
        // Anything that changes the driver goes first, so the agent starts under the final driver.
        assert!(!defers_until_started(&run(Verb::Setup(scope.clone()))));
        assert!(!defers_until_started(&run(Verb::InstallDriver)));
        assert!(!defers_until_started(&run(Verb::RemoveDriver)));
        assert!(!defers_until_started(&run(Verb::Teardown(scope))));
        // Nothing to run: nothing to wait for.
        for outcome in [
            ElevatedOutcome::NotNeeded,
            ElevatedOutcome::Declined,
            ElevatedOutcome::Failed("No.".to_owned()),
        ] {
            assert!(!defers_until_started(&ElevatedWork::Done(outcome)));
        }
    }

    #[test]
    fn a_skipped_or_declined_step_names_only_what_it_was_for() {
        let scope = rule_scope();
        let firewall = Verb::AddFirewall(scope.clone());
        assert_eq!(
            declined_line(&firewall),
            "Administrator approval was declined. Until the firewall rule is added, other computers may not reach this PC."
        );
        assert_eq!(
            declined_line(&Verb::InstallDriver),
            "Administrator approval was declined. Without the display driver, Crosspane mirrors windows in place."
        );
        assert_eq!(
            declined_line(&Verb::Setup(scope.clone())),
            format!("Administrator approval was declined. {INSTALL_WITHOUT_SETUP}")
        );
        assert_eq!(
            approval_pending(&firewall),
            "The firewall rule still needs administrator approval. Choose Repair to approve."
        );
        assert_eq!(
            approval_pending(&Verb::InstallDriver),
            "The display driver still needs administrator approval. Choose Repair to approve."
        );
        assert_eq!(approval_pending(&Verb::Setup(scope)), APPROVAL_PENDING);
    }

    #[test]
    fn a_declined_removal_says_what_was_kept() {
        let scope = rule_scope();
        assert_eq!(
            kept_note(&Verb::Teardown(scope.clone())),
            "Administrator approval was declined, so the firewall rule and display driver were kept."
        );
        assert_eq!(
            kept_note(&Verb::RemoveFirewall(scope)),
            "Administrator approval was declined, so the firewall rule was kept."
        );
        assert_eq!(
            kept_note(&Verb::RemoveDriver),
            "Administrator approval was declined, so the display driver was kept."
        );
        assert_eq!(admin_parts(&Verb::RemoveDriver), "display driver");
    }

    #[test]
    fn the_administrator_texts_stay_in_plain_words() {
        let scope = rule_scope();
        let verbs = [
            Verb::Setup(scope.clone()),
            Verb::AddFirewall(scope.clone()),
            Verb::InstallDriver,
            Verb::Teardown(scope.clone()),
            Verb::RemoveFirewall(scope),
            Verb::RemoveDriver,
        ];
        for verb in verbs {
            for text in [
                declined_line(&verb),
                approval_pending(&verb).to_owned(),
                kept_note(&verb),
            ] {
                let text = text.to_lowercase();
                for word in BANNED {
                    assert!(!text.contains(word), "{verb:?}: {text} mentions {word}");
                }
            }
        }
    }

    #[test]
    fn the_uninstall_preview_follows_the_settings_choice() {
        let keep = uninstall_preview(false);
        assert_eq!(keep.first().map(String::as_str), Some("Stop Crosspane."));
        assert!(keep.contains(&"Keep your settings and pairings.".to_owned()));
        assert!(!keep.contains(&"Delete your settings and pairings.".to_owned()));
        let delete = uninstall_preview(true);
        assert!(delete.contains(&"Delete your settings and pairings.".to_owned()));
        assert!(!delete.contains(&"Keep your settings and pairings.".to_owned()));
        for line in keep.iter().chain(&delete) {
            let line = line.to_lowercase();
            for word in BANNED {
                assert!(!line.contains(word), "{line} mentions {word}");
            }
        }
    }

    #[test]
    fn the_uninstall_summary_lists_what_is_left() {
        assert_eq!(
            uninstall_lines(&[]),
            vec!["Crosspane was uninstalled.".to_owned()]
        );
        assert_eq!(
            uninstall_lines(&[LEFT_TASK.to_owned(), LEFT_PROGRAMS.to_owned()]),
            vec![
                "Some parts are still there: sign-in task, program files.".to_owned(),
                "Try again.".to_owned()
            ]
        );
    }

    #[test]
    fn the_erase_receipt_decides() {
        let receipt = |result: &str| {
            format!(
                "{{\"schema_version\":1,\"result\":\"{result}\",\"reason\":null,\"key\":\"removed\",\"trust\":\"removed\"}}\n"
            )
        };
        assert!(receipt_confirms(receipt("removed").as_bytes()));
        assert!(receipt_confirms(receipt("already_absent").as_bytes()));
        for result in ["refused", "failed", "waiting"] {
            assert!(!receipt_confirms(receipt(result).as_bytes()), "{result}");
        }
        assert!(!receipt_confirms(b""));
        assert!(!receipt_confirms(b"not json\n"));
        assert!(!receipt_confirms(b"5\n"));
        let noisy = format!("starting\n{}trailing\n", receipt("removed"));
        assert!(receipt_confirms(noisy.as_bytes()));
    }

    #[test]
    fn only_crosspane_folders_are_deleted() {
        assert!(is_crosspane_dir(Path::new(
            r"C:\Users\x\AppData\Local\Programs\Crosspane"
        )));
        assert!(is_crosspane_dir(Path::new(
            r"C:\Users\x\AppData\Roaming\crosspane"
        )));
        assert!(!is_crosspane_dir(Path::new(r"C:\Users\x\AppData\Local")));
        assert!(!is_crosspane_dir(Path::new(r"C:\")));
        assert!(!is_crosspane_dir(Path::new(r"C:\Users\x\Crosspane-old")));
    }

    fn sample_paths() -> Paths {
        Paths::at(
            Path::new(r"C:\Users\test\AppData\Local"),
            Path::new(r"C:\Users\test\AppData\Roaming"),
            Path::new(r"C:\Users\test\AppData\Local\Temp"),
        )
        .unwrap()
    }

    #[test]
    fn the_folders_to_move_to_are_outside_the_program_folder_and_the_state_folder_is_last() {
        let paths = sample_paths();
        let temp = PathBuf::from(r"C:\Users\test\AppData\Local\Temp");
        let windows = PathBuf::from(r"C:\WINDOWS");
        assert_eq!(
            exit_candidates(&paths, Some(windows.clone()), temp.clone()),
            vec![windows, temp.clone(), paths.state.clone()]
        );
        // No system folder known: the others still work.
        assert_eq!(
            exit_candidates(&paths, None, temp.clone()),
            vec![temp, paths.state.clone()]
        );
        // Nothing inside the program folder is ever offered, however it is spelled.
        let inside = PathBuf::from(r"\\?\c:\users\TEST\appdata\local\programs\crosspane\sub");
        let candidates = exit_candidates(&paths, Some(inside.clone()), inside);
        assert_eq!(candidates, vec![paths.state.clone()]);
        for dir in candidates {
            assert!(!paths::is_under(&dir, &paths.install));
        }
    }

    #[test]
    fn hex_is_lowercase_and_full_width() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff");
        assert_eq!(hex(&[0u8; 32]).len(), 64);
    }
}
