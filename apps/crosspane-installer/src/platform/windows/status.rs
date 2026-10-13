//! The status snapshot behind `--status`, the wizard's steps and `--diagnose`.

use super::{
    agent_pipe::{self, Answer},
    elevated,
    files::{self, FileState, Inventory},
    legacy,
    paths::Paths,
    setup_log::setup_note,
    shell, state,
    state::Installed,
    supervisor, support,
    task::{self, TaskHealth},
};
use crate::{diagnose::Class, live::CheckState};
use crosspane_installer_core::elevated::{DriverState, FirewallState};
use std::{path::Path, time::Duration};

/// How long `--diagnose` waits for the agent to answer.
const DIAGNOSE_AGENT_WAIT: Duration = Duration::from_secs(3);

/// What the elevated helper's `status` could not tell us.
const UNKNOWN: &str = "unknown";

/// Everything `--status` prints, as one JSON object.
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct Snapshot {
    pub schema: u32,
    pub installer_version: String,
    pub install_dir: String,
    pub files: Vec<FileReport>,
    pub files_ok: bool,
    pub task: TaskHealth,
    pub supervisor: bool,
    pub agent: AgentReport,
    pub firewall: Option<String>,
    pub driver: Option<String>,
    pub start_menu: bool,
    pub uninstall_entry: bool,
    pub legacy_leftovers: bool,
    pub installed: Option<Installed>,
    pub healthy: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct FileReport {
    pub name: String,
    pub state: FileState,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct AgentReport {
    pub answering: bool,
    pub instance: Option<u64>,
    pub pid: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SnapshotOptions {
    pub elevated: bool,
    pub agent_timeout: Duration,
}

/// Reads the machine's state. Nothing here changes anything: it queries files, the task, the
/// shell entries, the agent's pipe and (when `options.elevated`) the helper's unelevated `status`.
pub(crate) fn snapshot(
    paths: &Paths,
    inventory: &Inventory,
    payload: Option<&Path>,
    options: SnapshotOptions,
) -> Snapshot {
    let files: Vec<FileReport> = files::inspect(paths, inventory)
        .into_iter()
        .map(|(file, state)| FileReport {
            name: file.relative().to_owned(),
            state,
        })
        .collect();
    // An empty list means nothing could be inspected, which is not "all files fine".
    let files_ok = !files.is_empty() && files.iter().all(|file| file.state == FileState::Ok);

    let answer = agent_pipe::wait_until_answering(paths, options.agent_timeout);
    let agent = AgentReport {
        answering: answer.is_some(),
        instance: answer.as_ref().and_then(Answer::instance),
        pid: answer.as_ref().map(|answer| answer.server_pid),
    };

    let (firewall, driver) = if options.elevated {
        elevated_states(paths, payload)
    } else {
        (None, None)
    };
    let shell = shell::observe(paths);

    let mut snapshot = Snapshot {
        schema: 1,
        installer_version: inventory.version().to_owned(),
        install_dir: paths.install.display().to_string(),
        files,
        files_ok,
        task: task::inspect(paths),
        supervisor: supervisor::running(),
        agent,
        firewall,
        driver,
        start_menu: shell.start_menu,
        uninstall_entry: shell.uninstall_entry,
        legacy_leftovers: !legacy::items(paths).is_empty(),
        installed: state::read(paths),
        healthy: false,
    };
    snapshot.healthy = snapshot.converged()
        && snapshot.agent.answering
        && snapshot
            .firewall
            .as_deref()
            .is_none_or(|state| state == "present");
    snapshot
}

impl Snapshot {
    /// `files_ok && task Healthy && start_menu && uninstall_entry && !legacy_leftovers`
    pub(crate) fn converged(&self) -> bool {
        self.files_ok
            && self.task == TaskHealth::Healthy
            && self.start_menu
            && self.uninstall_entry
            && !self.legacy_leftovers
    }
}

/// The firewall and driver states from the helper's unelevated `status`, as short snake_case words;
/// both `"unknown"` when the helper could not be asked or answered with something unreadable.
fn elevated_states(paths: &Paths, payload: Option<&Path>) -> (Option<String>, Option<String>) {
    let report = elevated::scope(paths)
        .map_err(|error| error.text)
        .and_then(|scope| elevated::observe(paths, payload, &scope));
    match report {
        Ok(report) => (
            Some(firewall_word(report.firewall.state).to_owned()),
            Some(driver_word(report.driver.state).to_owned()),
        ),
        Err(reason) => {
            setup_note!("status: the helper's status could not be read: {reason}");
            (Some(UNKNOWN.to_owned()), Some(UNKNOWN.to_owned()))
        }
    }
}

fn firewall_word(state: FirewallState) -> &'static str {
    match state {
        FirewallState::Present => "present",
        FirewallState::Missing => "missing",
        FirewallState::Mismatch => "mismatch",
        FirewallState::Unavailable => "unavailable",
        FirewallState::NotRequested => "not_requested",
    }
}

fn driver_word(state: DriverState) -> &'static str {
    match state {
        DriverState::Absent => "absent",
        DriverState::PackageOnly => "package_only",
        DriverState::DeviceWithoutDriver => "device_without_driver",
        DriverState::Installed => "installed",
        DriverState::Mismatch => "mismatch",
        DriverState::Unavailable => "unavailable",
    }
}

/// `--diagnose` on Windows: maps `Snapshot` fields onto the shared `crate::diagnose::Report`.
///
/// One row per area: support, files, sign-in task, whether Crosspane is running, firewall,
/// display driver, Start menu and Apps & Features entries, and leftovers of the older installer.
/// Only an unsupported machine is a hard stop; every other finding is something a run of setup
/// puts right.
pub(crate) fn diagnose(payload: Option<&Path>, report: &mut crate::diagnose::Report) {
    let support = support::check(payload);
    if support.supported {
        ok(
            report,
            "support",
            "this computer can run Crosspane",
            "supported",
        );
    } else {
        let reason = support
            .reason
            .as_deref()
            .unwrap_or("Crosspane Setup can't run on this computer.");
        problem(
            report,
            "support",
            "this computer can run Crosspane",
            reason,
            true,
        );
    }
    for check in &support.checks {
        support_row(report, &check.label, &check.state);
    }

    let paths = match Paths::current() {
        Ok(paths) => paths,
        Err(error) => {
            problem(report, "payload", "install location", &error.text, true);
            report.unavailable_target();
            return;
        }
    };
    let inventory = match Inventory::embedded() {
        Ok(inventory) => inventory,
        Err(error) => {
            problem(
                report,
                "payload",
                "list of Crosspane's files",
                &error.text,
                true,
            );
            return;
        }
    };
    let snapshot = snapshot(
        &paths,
        &inventory,
        payload,
        SnapshotOptions {
            elevated: true,
            agent_timeout: DIAGNOSE_AGENT_WAIT,
        },
    );
    snapshot_rows(report, &snapshot);
}

fn snapshot_rows(report: &mut crate::diagnose::Report, snapshot: &Snapshot) {
    let check = "Crosspane's files";
    if snapshot.files_ok {
        ok(report, "payload", check, "every file matches");
    } else {
        problem(
            report,
            "payload",
            check,
            &files_text(&snapshot.files),
            false,
        );
    }

    let check = "sign-in task";
    match snapshot.task {
        TaskHealth::Healthy => ok(report, "service", check, "registered for this account"),
        TaskHealth::Missing => problem(report, "service", check, "Not registered yet.", false),
        TaskHealth::Disabled => problem(report, "service", check, "Turned off in Windows.", false),
        TaskHealth::Different => problem(
            report,
            "service",
            check,
            "Registered, but it doesn't start Crosspane from the install folder.",
            false,
        ),
        TaskHealth::Unavailable => problem(
            report,
            "service",
            check,
            "Windows' task list couldn't be read.",
            false,
        ),
    }

    let check = "Crosspane running";
    if snapshot.agent.answering {
        let text = match snapshot.agent.pid {
            Some(pid) => format!("answering (process {pid})"),
            None => "answering".to_owned(),
        };
        ok(report, "agent", check, &text);
    } else if snapshot.supervisor {
        problem(
            report,
            "agent",
            check,
            "Crosspane's sign-in keeper is running, but Crosspane isn't answering.",
            false,
        );
    } else {
        problem(report, "agent", check, "Crosspane isn't running.", false);
    }

    elevated_row(
        report,
        "firewall rule",
        snapshot.firewall.as_deref(),
        &["present"],
    );
    elevated_row(
        report,
        "display driver",
        snapshot.driver.as_deref(),
        &["installed"],
    );

    if snapshot.start_menu {
        ok(report, "shell", "Start menu shortcuts", "in place");
    } else {
        problem(
            report,
            "shell",
            "Start menu shortcuts",
            "Not in place yet.",
            false,
        );
    }
    if snapshot.uninstall_entry {
        ok(report, "shell", "Apps & Features entry", "in place");
    } else {
        problem(
            report,
            "shell",
            "Apps & Features entry",
            "Not in place yet.",
            false,
        );
    }

    let check = "leftovers of an older installer";
    if snapshot.legacy_leftovers {
        problem(
            report,
            "cleanup",
            check,
            "Some are still around; the next setup run moves them into a backup folder.",
            false,
        );
    } else {
        ok(report, "cleanup", check, "none");
    }
}

/// One row for the firewall rule or the display driver: fine when the word is in `good`.
fn elevated_row(
    report: &mut crate::diagnose::Report,
    check: &str,
    word: Option<&str>,
    good: &[&str],
) {
    match word {
        Some(word) if good.contains(&word) => ok(report, "admin", check, &word.replace('_', " ")),
        Some(UNKNOWN) | None => problem(
            report,
            "admin",
            check,
            "Couldn't be read without administrator approval.",
            false,
        ),
        Some(word) => problem(
            report,
            "admin",
            check,
            &format!(
                "Reported as \"{}\"; setup needs administrator approval to fix it.",
                word.replace('_', " ")
            ),
            false,
        ),
    }
}

/// "crosspane-agent.exe is missing, crosspane-ui.exe differs" for every file that is not fine.
fn files_text(files: &[FileReport]) -> String {
    let bad: Vec<String> = files
        .iter()
        .filter(|file| file.state != FileState::Ok)
        .map(|file| match file.state {
            FileState::Missing => format!("{} is missing", file.name),
            _ => format!("{} differs from the approved copy", file.name),
        })
        .collect();
    if bad.is_empty() {
        "The installed files couldn't be checked.".to_owned()
    } else {
        format!("{}.", bad.join(", "))
    }
}

/// One support-checklist row: a failure is a hard stop, anything else is information.
fn support_row(report: &mut crate::diagnose::Report, label: &str, state: &CheckState) {
    match state {
        CheckState::Checking => ok(report, "support", label, "checking"),
        CheckState::Passed(Some(value)) => ok(report, "support", label, value),
        CheckState::Passed(None) => ok(report, "support", label, "ok"),
        CheckState::Note(text) => ok(report, "support", label, text),
        CheckState::Failed(reason) => problem(report, "support", label, reason, true),
        CheckState::Unconfirmed(reason) => problem(report, "support", label, reason, false),
    }
}

fn ok(report: &mut crate::diagnose::Report, step: &'static str, check: &str, text: &str) {
    report.record::<_, &str>(step, check, Class::S, Ok(text), false);
}

fn problem(
    report: &mut crate::diagnose::Report,
    step: &'static str,
    check: &str,
    text: &str,
    blocking: bool,
) {
    report.issue(step, check, Class::S, text, blocking);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::windows::paths::PayloadFile;

    fn snapshot_for_tests() -> Snapshot {
        Snapshot {
            schema: 1,
            installer_version: "0.1.0".to_owned(),
            install_dir: r"C:\Users\x\AppData\Local\Programs\Crosspane".to_owned(),
            files: vec![FileReport {
                name: "crosspane-agent.exe".to_owned(),
                state: FileState::Ok,
            }],
            files_ok: true,
            task: TaskHealth::Healthy,
            supervisor: true,
            agent: AgentReport {
                answering: true,
                instance: Some(7),
                pid: Some(1234),
            },
            firewall: Some("present".to_owned()),
            driver: Some("installed".to_owned()),
            start_menu: true,
            uninstall_entry: true,
            legacy_leftovers: false,
            installed: None,
            healthy: true,
        }
    }

    #[test]
    fn converged_needs_every_part() {
        let healthy = snapshot_for_tests();
        assert!(healthy.converged());

        let mut broken = healthy.clone();
        broken.files_ok = false;
        assert!(!broken.converged());
        let mut broken = healthy.clone();
        broken.task = TaskHealth::Different;
        assert!(!broken.converged());
        let mut broken = healthy.clone();
        broken.start_menu = false;
        assert!(!broken.converged());
        let mut broken = healthy.clone();
        broken.uninstall_entry = false;
        assert!(!broken.converged());
        let mut broken = healthy.clone();
        broken.legacy_leftovers = true;
        assert!(!broken.converged());
        // The agent and the firewall are not part of "converged" (setup puts files and the task in
        // place; the agent and the firewall are checked on top of it by `healthy`).
        let mut quiet = healthy;
        quiet.agent.answering = false;
        quiet.firewall = Some("missing".to_owned());
        assert!(quiet.converged());
    }

    #[test]
    fn state_words_are_snake_case() {
        assert_eq!(firewall_word(FirewallState::Present), "present");
        assert_eq!(firewall_word(FirewallState::NotRequested), "not_requested");
        assert_eq!(driver_word(DriverState::Installed), "installed");
        assert_eq!(driver_word(DriverState::PackageOnly), "package_only");
        assert_eq!(
            driver_word(DriverState::DeviceWithoutDriver),
            "device_without_driver"
        );
    }

    #[test]
    fn snapshot_serialises_to_one_line_with_the_frozen_keys() {
        let line = serde_json::to_string(&snapshot_for_tests()).unwrap_or_default();
        assert!(!line.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
        for key in [
            "schema",
            "installer_version",
            "install_dir",
            "files",
            "files_ok",
            "task",
            "supervisor",
            "agent",
            "firewall",
            "driver",
            "start_menu",
            "uninstall_entry",
            "legacy_leftovers",
            "installed",
            "healthy",
        ] {
            assert!(value.get(key).is_some(), "missing key {key}");
        }
        assert_eq!(value["task"], "healthy");
        assert_eq!(value["files"][0]["state"], "ok");
        assert_eq!(value["agent"]["pid"], 1234);
    }

    #[test]
    fn files_text_names_what_is_wrong() {
        let files = vec![
            FileReport {
                name: PayloadFile::Agent.relative().to_owned(),
                state: FileState::Missing,
            },
            FileReport {
                name: PayloadFile::Ui.relative().to_owned(),
                state: FileState::Different,
            },
            FileReport {
                name: PayloadFile::Ctl.relative().to_owned(),
                state: FileState::Ok,
            },
        ];
        assert_eq!(
            files_text(&files),
            "crosspane-agent.exe is missing, crosspane-ui.exe differs from the approved copy."
        );
    }
}
