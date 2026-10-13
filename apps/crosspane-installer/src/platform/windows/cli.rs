//! The headless command line (L14): argument routing and the CLI runner on the shared engine.

use super::{
    engine::{
        self, ConvergeOptions, ConvergeReport, ElevatedPolicy, Progress, Route, UninstallOptions,
        UninstallReport, text,
    },
    files::Inventory,
    paths::{self, Paths},
    setup_log::setup_note,
    status::{self, SnapshotOptions},
    supervisor, support,
};
use std::{
    ffi::OsString,
    io::Write as _,
    os::windows::io::IntoRawHandle as _,
    path::{Path, PathBuf},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        SetStdHandle,
    },
};

/// How long `--status` waits for the agent to answer.
const STATUS_AGENT_WAIT: Duration = Duration::from_secs(3);

/// What the person sees after a usage mistake.
const USAGE: &str = "\
Usage:
  crosspane-installer                         open Crosspane Setup
  crosspane-installer --install [--payload DIR] [--complete] [--skip-admin]
  crosspane-installer --repair
  crosspane-installer --uninstall [--remove-settings] [--teardown]
  crosspane-installer --status
  crosspane-installer --maintenance           open Crosspane Setup on the Repair or Remove screen";

/// One lifecycle command. `skip_admin` sets `ElevatedPolicy::Never` so a scripted install never
/// raises UAC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Install {
        payload: Option<PathBuf>,
        complete: bool,
        skip_admin: bool,
    },
    Repair,
    Uninstall {
        remove_settings: bool,
        teardown: bool,
    },
    Status,
}

/// What the process was started to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    Supervisor,
    Cli(Command),
    Maintenance,
    Gui,
}

/// The flags that choose what to do. At most one may be given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Supervisor,
    Install,
    Repair,
    Uninstall,
    Status,
    Maintenance,
}

impl Kind {
    fn from_flag(flag: &str) -> Option<Self> {
        match flag {
            supervisor::SUPERVISOR_FLAG | supervisor::SUPERVISOR_ALIAS => Some(Self::Supervisor),
            "--install" => Some(Self::Install),
            "--repair" => Some(Self::Repair),
            "--uninstall" => Some(Self::Uninstall),
            "--status" => Some(Self::Status),
            "--maintenance" => Some(Self::Maintenance),
            _ => None,
        }
    }

    fn flag(self) -> &'static str {
        match self {
            Self::Supervisor => supervisor::SUPERVISOR_FLAG,
            Self::Install => "--install",
            Self::Repair => "--repair",
            Self::Uninstall => "--uninstall",
            Self::Status => "--status",
            Self::Maintenance => "--maintenance",
        }
    }
}

/// The flags that adjust a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Modifier {
    Payload,
    Complete,
    SkipAdmin,
    RemoveSettings,
    Teardown,
}

impl Modifier {
    fn from_flag(flag: &str) -> Option<Self> {
        match flag {
            "--payload" => Some(Self::Payload),
            "--complete" => Some(Self::Complete),
            "--skip-admin" => Some(Self::SkipAdmin),
            "--remove-settings" => Some(Self::RemoveSettings),
            "--teardown" => Some(Self::Teardown),
            _ => None,
        }
    }

    fn flag(self) -> &'static str {
        match self {
            Self::Payload => "--payload",
            Self::Complete => "--complete",
            Self::SkipAdmin => "--skip-admin",
            Self::RemoveSettings => "--remove-settings",
            Self::Teardown => "--teardown",
        }
    }

    fn allowed_with(self, kind: Kind) -> bool {
        match self {
            Self::Payload | Self::Complete | Self::SkipAdmin => kind == Kind::Install,
            Self::RemoveSettings | Self::Teardown => kind == Kind::Uninstall,
        }
    }
}

/// Everything one pass over the arguments found.
#[derive(Debug, Default)]
struct Parsed {
    kind: Option<Kind>,
    payload: Option<PathBuf>,
    complete: bool,
    skip_admin: bool,
    remove_settings: bool,
    teardown: bool,
    modifiers: Vec<Modifier>,
    /// The first thing wrong with the arguments, in plain words.
    problem: Option<String>,
}

impl Parsed {
    fn fault(&mut self, text: String) {
        self.problem.get_or_insert(text);
    }

    fn lifecycle(&mut self, kind: Kind) {
        match self.kind {
            None => self.kind = Some(kind),
            Some(first) if first == kind => {
                self.fault(format!("{} was given more than once.", kind.flag()));
            }
            Some(first) => self.fault(format!(
                "{} and {} can't be used together.",
                first.flag(),
                kind.flag()
            )),
        }
    }

    /// Records that `modifier` was given; a second time is a mistake.
    fn modifier(&mut self, modifier: Modifier) {
        if self.modifiers.contains(&modifier) {
            self.fault(format!("{} was given more than once.", modifier.flag()));
        } else {
            self.modifiers.push(modifier);
        }
    }

    fn payload_value(&mut self, value: Option<OsString>) {
        self.modifier(Modifier::Payload);
        match value.filter(|value| !value.is_empty()) {
            Some(value) => {
                self.payload.get_or_insert_with(|| PathBuf::from(value));
            }
            None => self.fault("--payload needs the folder that holds the Crosspane files.".into()),
        }
    }
}

fn usage(problem: &str) -> String {
    format!("{problem}\n{USAGE}")
}

/// Strict: a lifecycle flag plus any unknown flag, a duplicate flag or a second lifecycle flag →
/// `Err(usage)`. `--maintenance` → `Maintenance`. `--payload P` is allowed with `--install` and
/// with no lifecycle flag (`Gui`, where the GUI's own flags pass through unchecked and clap
/// validates them later).
pub fn parse(arguments: &[OsString]) -> Result<Launch, String> {
    if supervisor::is_supervisor(arguments) {
        return Ok(Launch::Supervisor);
    }

    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        index += 1;
        let Some(text) = argument.to_str() else {
            parsed.fault(format!("Unknown option {}.", argument.to_string_lossy()));
            continue;
        };
        if let Some(kind) = Kind::from_flag(text) {
            parsed.lifecycle(kind);
        } else if text == "--payload" {
            // The value is the next argument, unless that is another option.
            let value = arguments
                .get(index)
                .filter(|next| !next.to_string_lossy().starts_with("--"))
                .cloned();
            if value.is_some() {
                index += 1;
            }
            parsed.payload_value(value);
        } else if let Some(value) = text.strip_prefix("--payload=") {
            parsed.payload_value(Some(OsString::from(value)));
        } else if let Some(modifier) = Modifier::from_flag(text) {
            parsed.modifier(modifier);
            match modifier {
                Modifier::Complete => parsed.complete = true,
                Modifier::SkipAdmin => parsed.skip_admin = true,
                Modifier::RemoveSettings => parsed.remove_settings = true,
                Modifier::Teardown => parsed.teardown = true,
                Modifier::Payload => {}
            }
        } else {
            parsed.fault(format!("Unknown option {text}."));
        }
    }

    // No lifecycle flag: this is the GUI, and clap checks its flags later.
    let Some(kind) = parsed.kind else {
        return Ok(Launch::Gui);
    };
    if let Some(problem) = &parsed.problem {
        return Err(usage(problem));
    }
    if let Some(stray) = parsed.modifiers.iter().find(|m| !m.allowed_with(kind)) {
        return Err(usage(&format!(
            "{} can't be used with {}.",
            stray.flag(),
            kind.flag()
        )));
    }
    match kind {
        // The supervisor flag on its own was handled above; anything else is a mistake.
        Kind::Supervisor => Err(usage(&format!(
            "{} must be the only argument.",
            supervisor::SUPERVISOR_FLAG
        ))),
        Kind::Install => Ok(Launch::Cli(Command::Install {
            payload: parsed.payload,
            complete: parsed.complete,
            skip_admin: parsed.skip_admin,
        })),
        Kind::Repair => Ok(Launch::Cli(Command::Repair)),
        Kind::Uninstall => Ok(Launch::Cli(Command::Uninstall {
            remove_settings: parsed.remove_settings,
            teardown: parsed.teardown,
        })),
        Kind::Status => Ok(Launch::Cli(Command::Status)),
        Kind::Maintenance => Ok(Launch::Maintenance),
    }
}

/// Reports a usage mistake from `parse` on the parent console and returns the exit code, 2.
pub fn usage_exit(message: &str) -> i32 {
    attach_console();
    err(message);
    2
}

/// Attaches the parent console only if stdout/stderr are not inherited; refuses an unsupported
/// machine (`support::check`); progress → stderr, result → stdout (`Status`: one JSON object);
/// 0 ok, 1 failure, 2 usage. `Install`: `ElevatedPolicy::Never` if `skip_admin`, else `IfNeeded`.
/// `Uninstall`: `teardown` → `IfNeeded`, else `Never`.
pub fn run(command: Command) -> i32 {
    attach_console();
    let name = match &command {
        Command::Install { .. } => "install",
        Command::Repair => "repair",
        Command::Uninstall { .. } => "uninstall",
        Command::Status => "status",
    };
    setup_note!("cli: {name} started");
    let code = match command {
        Command::Install {
            payload,
            complete,
            skip_admin,
        } => converge_command(ConvergeOptions {
            payload: payload.or_else(paths::payload_beside_current_exe),
            route: if complete {
                Route::CompleteReinstall
            } else {
                Route::Install
            },
            elevated: if skip_admin {
                ElevatedPolicy::Never
            } else {
                ElevatedPolicy::IfNeeded
            },
        }),
        Command::Repair => converge_command(ConvergeOptions {
            payload: paths::payload_beside_current_exe(),
            route: Route::Repair,
            elevated: ElevatedPolicy::IfNeeded,
        }),
        Command::Uninstall {
            remove_settings,
            teardown,
        } => uninstall_command(UninstallOptions {
            remove_settings,
            teardown: if teardown {
                ElevatedPolicy::IfNeeded
            } else {
                ElevatedPolicy::Never
            },
        }),
        Command::Status => status_command(),
    };
    setup_note!("cli: {name} finished with exit code {code}");
    code
}

/// Install, Complete reinstall and Repair: the same converge with different options.
fn converge_command(options: ConvergeOptions) -> i32 {
    let Some((paths, inventory)) = prepare(options.payload.as_deref()) else {
        return 1;
    };
    match engine::converge(&paths, &inventory, &options, &StderrProgress) {
        Ok(report) => finish_converge(&report),
        Err(error) => fail(&error.text),
    }
}

/// Prints what the converge did, then `ok` or `failed: …`. The administrator step's outcome is
/// already one of `report.lines` (the engine adds it), so it is not repeated here.
fn finish_converge(report: &ConvergeReport) -> i32 {
    for line in &report.lines {
        out(line);
    }
    if report.ok() {
        out("ok");
        0
    } else {
        fail(&converge_failure(report))
    }
}

/// The first thing a finished converge did not achieve, in plain words.
fn converge_failure(report: &ConvergeReport) -> String {
    if !report.files_ok {
        "Crosspane's files aren't all in place.".to_owned()
    } else if !report.task_ok {
        "Crosspane's sign-in task isn't set up.".to_owned()
    } else if !report.agent_answering {
        text::NOT_STARTED_YET.to_owned()
    } else {
        "Setup didn't finish.".to_owned()
    }
}

fn uninstall_command(options: UninstallOptions) -> i32 {
    let Some((paths, inventory)) = prepare(None) else {
        return 1;
    };
    match engine::uninstall(&paths, &inventory, &options, &StderrProgress) {
        Ok(report) => finish_uninstall(&report),
        Err(error) => fail(&error.text),
    }
}

fn finish_uninstall(report: &UninstallReport) -> i32 {
    for line in &report.lines {
        out(line);
    }
    if report.left.is_empty() {
        out("ok");
        0
    } else {
        fail(&format!(
            "Left in place: {}. Close Crosspane and run the uninstall again.",
            report.left.join(", ")
        ))
    }
}

/// `--status`: one line of JSON on stdout. An unsupported computer is mentioned on stderr, but the
/// snapshot is still printed whenever the install folders and file list can be resolved.
fn status_command() -> i32 {
    let payload = paths::payload_beside_current_exe();
    let support = support::check(payload.as_deref());
    if !support.supported {
        err(&unsupported_text(support.reason));
    }
    let (paths, inventory) = match resolve() {
        Ok(resolved) => resolved,
        Err(text) => {
            err(&text);
            return 1;
        }
    };
    let snapshot = status::snapshot(
        &paths,
        &inventory,
        payload.as_deref(),
        SnapshotOptions {
            elevated: true,
            agent_timeout: STATUS_AGENT_WAIT,
        },
    );
    match serde_json::to_string(&snapshot) {
        Ok(line) => {
            out(&line);
            0
        }
        Err(_) => {
            err("The status couldn't be written out.");
            1
        }
    }
}

/// Refuses an unsupported computer, then resolves the install folders and the file list. Prints
/// the reason to stderr and returns `None` when it cannot go on.
fn prepare(payload: Option<&Path>) -> Option<(Paths, Inventory)> {
    let support = support::check(payload);
    if !support.supported {
        err(&unsupported_text(support.reason));
        return None;
    }
    match resolve() {
        Ok(resolved) => Some(resolved),
        Err(text) => {
            err(&text);
            None
        }
    }
}

fn resolve() -> Result<(Paths, Inventory), String> {
    let paths = Paths::current().map_err(|error| error.text)?;
    let inventory = Inventory::embedded().map_err(|error| error.text)?;
    Ok((paths, inventory))
}

fn unsupported_text(reason: Option<String>) -> String {
    reason.unwrap_or_else(|| "Crosspane Setup can't run on this computer.".to_owned())
}

/// Ends the output with `failed: <text>`; the exit code is 1.
fn fail(text: &str) -> i32 {
    out(&format!("failed: {text}"));
    1
}

/// Each step goes to stderr as a line of its own.
#[derive(Debug)]
struct StderrProgress;

impl Progress for StderrProgress {
    fn step(&self, text: &str) {
        err(text);
    }
}

/// A closed pipe or a missing console must never abort the command, so write errors are dropped.
fn out(line: &str) {
    let _ = writeln!(std::io::stdout().lock(), "{line}");
}

fn err(line: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}

/// Whether the standard handle `id` is a usable handle.
fn std_handle_present(id: u32) -> bool {
    // SAFETY: GetStdHandle reads this process's standard-handle table and takes no pointers.
    let handle = unsafe { GetStdHandle(id) };
    !handle.is_null() && handle != INVALID_HANDLE_VALUE
}

/// Attaches to the parent's console, but only when stdout/stderr were not inherited: this is a
/// GUI-subsystem program, so a command started from a shell would otherwise print nothing. When
/// the attach leaves a standard handle empty, that handle is pointed at the console. Same logic as
/// the agent's `attach_console_for_cli`.
fn attach_console() {
    if std_handle_present(STD_OUTPUT_HANDLE) && std_handle_present(STD_ERROR_HANDLE) {
        return;
    }
    // SAFETY: AttachConsole takes a process id and no pointers. Failure (no parent console, for
    // example when started from Explorer) is expected and leaves the process as it was.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        return;
    }
    for id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if std_handle_present(id) {
            continue;
        }
        let Ok(console) = std::fs::OpenOptions::new().write(true).open("CONOUT$") else {
            continue;
        };
        // SAFETY: the handle is an open console output handle. It is released from its owner on
        // purpose, so it stays open for the life of the process as the standard handle.
        unsafe { SetStdHandle(id, console.into_raw_handle()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt as _;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn install(payload: Option<&str>, complete: bool, skip_admin: bool) -> Launch {
        Launch::Cli(Command::Install {
            payload: payload.map(PathBuf::from),
            complete,
            skip_admin,
        })
    }

    #[test]
    fn supervisor_flag_and_alias_alone() {
        assert_eq!(
            parse(&args(&["--windows-supervisor"])),
            Ok(Launch::Supervisor)
        );
        assert_eq!(parse(&args(&["--supervisor"])), Ok(Launch::Supervisor));
    }

    #[test]
    fn supervisor_flag_with_anything_else_is_refused() {
        assert!(parse(&args(&["--supervisor", "--status"])).is_err());
        assert!(parse(&args(&["--windows-supervisor", "extra"])).is_err());
        assert!(parse(&args(&["--supervisor", "--windows-supervisor"])).is_err());
        assert!(parse(&args(&["--supervisor", "--payload", "kit"])).is_err());
    }

    #[test]
    fn install_forms() {
        assert_eq!(
            parse(&args(&["--install"])),
            Ok(install(None, false, false))
        );
        assert_eq!(
            parse(&args(&["--install", "--payload", r"C:\kit"])),
            Ok(install(Some(r"C:\kit"), false, false))
        );
        assert_eq!(
            parse(&args(&["--install", "--payload=C:\\kit"])),
            Ok(install(Some(r"C:\kit"), false, false))
        );
        assert_eq!(
            parse(&args(&["--install", "--complete"])),
            Ok(install(None, true, false))
        );
        assert_eq!(
            parse(&args(&["--install", "--skip-admin"])),
            Ok(install(None, false, true))
        );
        // Order does not matter and everything combines.
        assert_eq!(
            parse(&args(&[
                "--skip-admin",
                "--payload",
                r"D:\payload",
                "--complete",
                "--install"
            ])),
            Ok(install(Some(r"D:\payload"), true, true))
        );
    }

    #[test]
    fn install_payload_needs_a_value() {
        assert!(parse(&args(&["--install", "--payload"])).is_err());
        assert!(parse(&args(&["--install", "--payload="])).is_err());
        // An option is never taken as the folder.
        assert!(parse(&args(&["--install", "--payload", "--complete"])).is_err());
        assert!(parse(&args(&["--payload", "--install"])).is_err());
    }

    #[test]
    fn install_payload_may_be_a_path_windows_cannot_print() {
        let odd = OsString::from_wide(&[0x43, 0x3a, 0x5c, 0xd800, 0x6b]);
        let arguments = vec![
            OsString::from("--install"),
            OsString::from("--payload"),
            odd.clone(),
        ];
        assert_eq!(
            parse(&arguments),
            Ok(Launch::Cli(Command::Install {
                payload: Some(PathBuf::from(odd)),
                complete: false,
                skip_admin: false,
            }))
        );
    }

    #[test]
    fn duplicates_are_refused() {
        assert!(parse(&args(&["--install", "--install"])).is_err());
        assert!(parse(&args(&["--install", "--complete", "--complete"])).is_err());
        assert!(parse(&args(&["--install", "--skip-admin", "--skip-admin"])).is_err());
        assert!(parse(&args(&["--install", "--payload", "a", "--payload", "b"])).is_err());
        assert!(parse(&args(&["--install", "--payload", "a", "--payload=b"])).is_err());
        assert!(parse(&args(&["--uninstall", "--teardown", "--teardown"])).is_err());
        assert!(
            parse(&args(&[
                "--uninstall",
                "--remove-settings",
                "--remove-settings"
            ]))
            .is_err()
        );
        assert!(parse(&args(&["--status", "--status"])).is_err());
    }

    #[test]
    fn repair_status_and_maintenance_take_no_options() {
        assert_eq!(
            parse(&args(&["--repair"])),
            Ok(Launch::Cli(Command::Repair))
        );
        assert_eq!(
            parse(&args(&["--status"])),
            Ok(Launch::Cli(Command::Status))
        );
        assert_eq!(parse(&args(&["--maintenance"])), Ok(Launch::Maintenance));
        for lifecycle in ["--repair", "--status", "--maintenance"] {
            for stray in [
                &["--payload", "kit"][..],
                &["--complete"],
                &["--skip-admin"],
                &["--remove-settings"],
                &["--teardown"],
            ] {
                let mut arguments = args(&[lifecycle]);
                arguments.extend(args(stray));
                assert!(parse(&arguments).is_err(), "{lifecycle} {stray:?}");
            }
        }
    }

    #[test]
    fn uninstall_forms() {
        let uninstall = |remove_settings, teardown| {
            Launch::Cli(Command::Uninstall {
                remove_settings,
                teardown,
            })
        };
        assert_eq!(parse(&args(&["--uninstall"])), Ok(uninstall(false, false)));
        assert_eq!(
            parse(&args(&["--uninstall", "--remove-settings"])),
            Ok(uninstall(true, false))
        );
        assert_eq!(
            parse(&args(&["--uninstall", "--teardown"])),
            Ok(uninstall(false, true))
        );
        assert_eq!(
            parse(&args(&["--teardown", "--uninstall", "--remove-settings"])),
            Ok(uninstall(true, true))
        );
        // Install-only options do not belong to uninstall, and the other way around.
        assert!(parse(&args(&["--uninstall", "--complete"])).is_err());
        assert!(parse(&args(&["--uninstall", "--payload", "kit"])).is_err());
        assert!(parse(&args(&["--install", "--teardown"])).is_err());
        assert!(parse(&args(&["--install", "--remove-settings"])).is_err());
    }

    #[test]
    fn a_lifecycle_flag_with_an_unknown_flag_is_refused() {
        assert!(parse(&args(&["--install", "--bogus"])).is_err());
        assert!(parse(&args(&["--bogus", "--status"])).is_err());
        assert!(parse(&args(&["--repair", "stray"])).is_err());
        assert!(parse(&args(&["--uninstall", "--demo"])).is_err());
        assert!(parse(&args(&["--install", "--complete=yes"])).is_err());
        let message = parse(&args(&["--status", "--bogus"])).unwrap_err();
        assert!(message.contains("--bogus"));
        assert!(message.contains("Usage"));
    }

    #[test]
    fn two_lifecycle_flags_are_refused() {
        assert!(parse(&args(&["--install", "--repair"])).is_err());
        assert!(parse(&args(&["--repair", "--uninstall"])).is_err());
        assert!(parse(&args(&["--status", "--maintenance"])).is_err());
        assert!(parse(&args(&["--maintenance", "--install"])).is_err());
    }

    #[test]
    fn no_lifecycle_flag_is_the_gui_and_nothing_is_validated() {
        assert_eq!(parse(&[]), Ok(Launch::Gui));
        assert_eq!(parse(&args(&["--payload", r"C:\kit"])), Ok(Launch::Gui));
        assert_eq!(parse(&args(&["--payload"])), Ok(Launch::Gui));
        assert_eq!(
            parse(&args(&[
                "--demo", "--screen", "welcome", "--size", "800x600"
            ])),
            Ok(Launch::Gui)
        );
        assert_eq!(parse(&args(&["--diagnose"])), Ok(Launch::Gui));
        // clap, not this parser, rejects these later.
        assert_eq!(parse(&args(&["--complete"])), Ok(Launch::Gui));
        assert_eq!(parse(&args(&["--bogus"])), Ok(Launch::Gui));
        assert_eq!(
            parse(&args(&["--payload", "a", "--payload", "b"])),
            Ok(Launch::Gui)
        );
    }
}
