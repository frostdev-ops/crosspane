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
    lock::OperationLock,
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
    status::StatusReport,
    step::{ElevatedPlan, INSTALL_DECLINE_NOTE, INSTALL_WITHOUT_SETUP},
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

/// A `Progress` that drops every line.
#[derive(Debug)]
pub(crate) struct NoProgress;

impl Progress for NoProgress {
    fn step(&self, _: &str) {}
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

impl ConvergePlan {
    /// The preview text, plus the elevated `preview_block` and `INSTALL_DECLINE_NOTE` when needed.
    ///
    /// The core `PREVIEW_HEADER` appears only when an administrator step is planned: that is what
    /// keeps the wizard from approving a prompt on its own.
    pub(crate) fn preview(&self, route: Route) -> String {
        let mut lines: Vec<String> = Vec::new();
        lines.push(
            match route {
                Route::Install => "Install Crosspane for this account.",
                Route::Repair => "Repair Crosspane.",
                Route::CompleteReinstall => "Reinstall all of Crosspane's files.",
            }
            .to_owned(),
        );
        if self.restart {
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
            lines.push(INSTALL_DECLINE_NOTE.to_owned());
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
        policy => match observe_scope(paths, payload, None, elevated::setup_needed) {
            Err(reason) => (None, Some(reason)),
            Ok((_, false)) => (None, None),
            Ok((scope, true)) => match policy {
                ElevatedPolicy::IfNeeded => (Some(elevated::plan(Verb::Setup(scope))?), None),
                _ => (None, Some(APPROVAL_PENDING.to_owned())),
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

/// Reads the helper's status for `scope` (found when `None`). `needed` says whether that status
/// calls for the administrator step. The error is plain text for the person using setup.
fn observe_scope(
    paths: &Paths,
    payload: Option<&Path>,
    scope: Option<RuleScope>,
    needed: fn(&StatusReport) -> bool,
) -> Result<(RuleScope, bool), String> {
    let scope = match scope {
        Some(scope) => scope,
        None => elevated::scope(paths).map_err(|error| error.text)?,
    };
    let report = elevated::observe(paths, payload, &scope)?;
    let needed = needed(&report);
    Ok((scope, needed))
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

/// lock → sources → plan → [stop] → `legacy::clean` → `files::place` → task (if !Healthy || route
/// != Install) → `shell::ensure` → elevated per policy → start (`task::run`, fallback direct
/// spawn) → wait ≤45 s → `state::write` (only if `files_ok && task_ok`) → `legacy::prune(5)`.
/// Idempotent; reads no prior state.
pub(crate) fn converge(
    paths: &Paths,
    inventory: &Inventory,
    options: &ConvergeOptions,
    progress: &dyn Progress,
) -> SetupResult<ConvergeReport> {
    let _lock = OperationLock::acquire()?;
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
    match files::place(
        paths,
        inventory,
        &sources,
        route == Route::CompleteReinstall,
        &mut backup,
    )? {
        FilesOutcome::Unchanged => setup_note!("converge: files unchanged"),
        FilesOutcome::Replaced(replaced) => {
            setup_note!("converge: replaced {} files", replaced.len());
        }
        FilesOutcome::NeedsDownloadedInstaller(missing) => {
            setup_note!("converge: {} files cannot be restored here", missing.len());
            needs_download = true;
            lines.push(text::NEEDS_DOWNLOADED_INSTALLER.to_owned());
        }
    }
    let states = files::inspect(paths, inventory);
    let file_ok = |wanted: PayloadFile| {
        states
            .iter()
            .any(|(file, state)| *file == wanted && *state == FileState::Ok)
    };
    let files_ok = !states.is_empty() && states.iter().all(|(_, state)| *state == FileState::Ok);
    if !files_ok && !needs_download {
        lines.push("Some of Crosspane's files are still missing or different.".to_owned());
    }

    let mut task_reported = false;
    if task::inspect(paths) != TaskHealth::Healthy || route != Route::Install {
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
    let task_ok = task::inspect(paths) == TaskHealth::Healthy;
    if !task_ok && !task_reported {
        lines.push("Crosspane isn't set to start when you sign in yet.".to_owned());
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
    let elevated = run_elevated(
        paths,
        inventory,
        options,
        payload.as_deref(),
        &plan,
        progress,
        &mut lines,
    );

    // Crosspane is started whenever the sign-in task is right and the two programs that run (the
    // agent and the supervisor, which is this installer) match their approved files. A damaged
    // file that can't be restored here (the interface, say) must not leave Crosspane stopped.
    let mut agent_answering = false;
    if task_ok && file_ok(PayloadFile::Agent) && file_ok(PayloadFile::Installer) {
        agent_answering = start_inner(paths, progress);
        if !agent_answering {
            lines.push(text::NOT_STARTED_YET.to_owned());
        }
    }
    if files_ok && task_ok {
        write_state(paths, inventory, &states, &mut lines);
    }

    legacy::prune(paths, BACKUPS_KEPT);

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
        "converge: done files_ok={} task_ok={} agent_answering={}",
        report.files_ok,
        report.task_ok,
        report.agent_answering
    );
    Ok(report)
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

/// `installed.json`: informational only, written last. A failure is a line, never a stop.
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

/// The administrator step, per policy. Never fatal: its outcome goes in the report.
fn run_elevated(
    paths: &Paths,
    inventory: &Inventory,
    options: &ConvergeOptions,
    payload: Option<&Path>,
    plan: &ConvergePlan,
    progress: &dyn Progress,
    lines: &mut Vec<String>,
) -> ElevatedOutcome {
    let consented = match &options.elevated {
        ElevatedPolicy::Never => {
            if let Some(note) = &plan.elevated_note {
                lines.push(note.clone());
            }
            return ElevatedOutcome::NotNeeded;
        }
        ElevatedPolicy::IfNeeded => None,
        ElevatedPolicy::Consented(consented) => Some(consented),
    };
    let known_scope = consented.and_then(|plan| plan.verb().scope().cloned());
    let (scope, needed) = match observe_scope(paths, payload, known_scope, elevated::setup_needed) {
        Ok(found) => found,
        Err(reason) => {
            setup_note!("converge: administrator step not checked: {reason}");
            lines.push(reason.clone());
            return ElevatedOutcome::Failed(reason);
        }
    };
    if !needed {
        return ElevatedOutcome::NotNeeded;
    }
    let to_run = match consented {
        Some(consented) => consented.clone(),
        None => match elevated::plan(Verb::Setup(scope)) {
            Ok(planned) => planned,
            Err(error) => {
                lines.push(error.text.clone());
                return ElevatedOutcome::Failed(error.text);
            }
        },
    };
    let Some(kit) = inventory.kit() else {
        let reason = "This installer can't set up the firewall rule and display driver.";
        lines.push(reason.to_owned());
        return ElevatedOutcome::Failed(reason.to_owned());
    };
    // `elevated::run` tells the person it is waiting for approval, and keeps saying so.
    let outcome = elevated::run(paths, kit, &to_run, progress);
    setup_note!("converge: administrator step {}", outcome_name(&outcome));
    match &outcome {
        ElevatedOutcome::NotNeeded => {}
        ElevatedOutcome::Configured(summary) => lines.push(summary.clone()),
        ElevatedOutcome::Declined => {
            lines.push(format!(
                "Administrator approval was declined. {INSTALL_WITHOUT_SETUP}"
            ));
        }
        ElevatedOutcome::Failed(reason) => lines.push(reason.clone()),
    }
    outcome
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

/// Lock; if `!supervisor::running()`: `task::run` (fallback spawn); wait ≤45 s.
pub(crate) fn start(paths: &Paths, progress: &dyn Progress) -> SetupResult<bool> {
    let _lock = OperationLock::acquire()?;
    if !paths.file(PayloadFile::Agent).is_file() {
        return Err(SetupError::before(text::NOT_INSTALLED));
    }
    Ok(start_inner(paths, progress))
}

/// Starts the sign-in supervisor unless one already runs, then waits for the agent to answer.
/// The caller holds the operation lock.
fn start_inner(paths: &Paths, progress: &dyn Progress) -> bool {
    if supervisor::running() {
        setup_note!("start: the supervisor is already running");
    } else {
        progress.step("Starting Crosspane…");
        launch_supervisor(paths);
    }
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
        match observe_scope(paths, None, None, elevated::teardown_needed) {
            Ok((scope, true)) => {
                let planned = elevated::plan(Verb::Teardown(scope))?;
                lines.push(planned.preview_block());
                plan = Some(planned);
            }
            Ok((_, false)) => {}
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
const LEFT_ADMIN: &str = "firewall rule and display driver";
const KEY_NOT_ERASED: &str = "Your settings were removed but the sign-in key couldn't be erased.";
const ADMIN_NOT_CHECKED: &str =
    "The firewall rule and display driver can't be checked right now, so they stay as they are.";

/// lock → stop → `task::delete` → [teardown] → [erase-identity if `remove_settings`] →
/// `shell::remove` → remove the install dir (move our own exe to backups or temp first) → [remove
/// the state and config dirs].
pub(crate) fn uninstall(
    paths: &Paths,
    inventory: &Inventory,
    options: &UninstallOptions,
    progress: &dyn Progress,
) -> SetupResult<UninstallReport> {
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

/// The summary lines of an uninstall: done, or what is still there with a retry.
fn uninstall_lines(left: &[String]) -> Vec<String> {
    if left.is_empty() {
        vec!["Crosspane was uninstalled.".to_owned()]
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
    let (scope, needed) = match observe_scope(paths, None, known_scope, elevated::teardown_needed) {
        Ok(found) => found,
        Err(reason) => {
            // Not "left": nothing a retry could fix, and it must never keep an uninstall open.
            setup_note!("uninstall: firewall rule and driver not checked: {reason}");
            notes.push(ADMIN_NOT_CHECKED.to_owned());
            return;
        }
    };
    if !needed {
        return;
    }
    let to_run = match consented {
        Some(consented) => consented.clone(),
        None => match elevated::plan(Verb::Teardown(scope)) {
            Ok(planned) => planned,
            Err(error) => {
                setup_note!("uninstall: teardown plan failed: {}", error.text);
                left.push(LEFT_ADMIN.to_owned());
                return;
            }
        },
    };
    let Some(kit) = inventory.kit() else {
        left.push(LEFT_ADMIN.to_owned());
        return;
    };
    progress.step("Removing the firewall rule and display driver…");
    let outcome = elevated::run(paths, kit, &to_run, progress);
    setup_note!("uninstall: administrator step {}", outcome_name(&outcome));
    match outcome {
        ElevatedOutcome::NotNeeded | ElevatedOutcome::Configured(_) => {}
        ElevatedOutcome::Declined => {
            left.push(LEFT_ADMIN.to_owned());
            notes.push("Administrator approval was declined, so the firewall rule and display driver were kept.".to_owned());
        }
        ElevatedOutcome::Failed(reason) => {
            left.push(LEFT_ADMIN.to_owned());
            notes.push(reason);
        }
    }
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
    use crosspane_installer_core::elevated::{AgentProgram, InstallId, step::PREVIEW_HEADER};

    const BANNED: [&str; 7] = [
        "foreign",
        "unsettled",
        "admission",
        "submission",
        "epoch",
        "journal",
        "outcomeunknown",
    ];

    fn setup_plan() -> ElevatedPlan {
        let scope = RuleScope {
            id: InstallId::parse("0123456789abcdef").unwrap(),
            program: AgentProgram::parse(
                r"C:\Users\someone\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
            )
            .unwrap(),
        };
        ElevatedPlan::new(Verb::Setup(scope)).unwrap()
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

    #[test]
    fn the_preview_carries_the_approval_header_only_with_an_administrator_plan() {
        for route in ROUTES {
            assert!(!has_header(&plain_plan().preview(route)));
            assert!(!has_header(&full_plan(None).preview(route)));
            let with = full_plan(Some(setup_plan())).preview(route);
            assert!(has_header(&with), "{route:?}");
            assert!(with.contains(INSTALL_DECLINE_NOTE), "{route:?}");
        }
        let without = full_plan(None).preview(Route::Install);
        assert!(!without.contains(INSTALL_DECLINE_NOTE));
    }

    #[test]
    fn the_preview_stays_in_plain_words() {
        for route in ROUTES {
            for plan in [plain_plan(), full_plan(None), full_plan(Some(setup_plan()))] {
                let preview = plan.preview(route).to_lowercase();
                for word in BANNED {
                    assert!(!preview.contains(word), "{route:?} mentions {word}");
                }
            }
        }
    }

    #[test]
    fn the_preview_fits_the_controller_limit() {
        for route in ROUTES {
            let preview = full_plan(Some(setup_plan())).preview(route);
            assert!(preview.len() <= 2_400, "{} bytes", preview.len());
        }
    }

    #[test]
    fn the_preview_lists_what_will_happen() {
        let preview = full_plan(None).preview(Route::Install);
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

    #[test]
    fn hex_is_lowercase_and_full_width() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff");
        assert_eq!(hex(&[0u8; 32]).len(), 64);
    }
}
