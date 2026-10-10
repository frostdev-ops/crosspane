//! GNOME and KDE lifecycle evidence from the selected user's own systemd manager, the way
//! `read` judges uwsm: effective unit properties, never a unit name, an exported variable or an
//! active target alone. The compositor that serves this session must be the running main
//! process of its session-bound unit; support assembly then correlates that PID with the Wayland
//! socket's peer. Nothing here is a readiness claim.
use super::*;

const KWIN: &str = "/usr/bin/kwin_wayland";
const SHELL_PREFIX: &str = "org.gnome.Shell@";
const SHELL_PATTERN: &str = "org.gnome.Shell@*.service";
const SESSION_PATTERN: &str = "gnome-session@*.target";
const KWIN_UNIT: &str = "plasma-kwin_wayland.service";
/// The only GNOME Shell executable accepted.
pub const GNOME_SHELL: &str = "/usr/bin/gnome-shell";
/// Instances of the Shell unit that belong to a user session (`@user` since GNOME 49, `@wayland`
/// before). The greeter's `@gdm` lives in another account's manager and is never accepted.
const SHELL_INSTANCES: [&str; 2] = ["user", "wayland"];

/// A service's running main process and its single `ExecStart` record.
struct Running {
    kind: String,
    pid: u32,
    path: String,
    argv: Vec<String>,
    ignore_errors: bool,
}

/// The same rules `running_uwsm` applies to the unit's `Type`, `MainPID` and `ExecStart`: one
/// command record, either populated and running or (systemd 261) empty with the `ExecMain*`
/// properties showing the running main process.
fn running_service(values: &Properties) -> Result<Running, ProbeIssue> {
    let kind: String = required(values, "Type")?;
    bounded_text(&kind, 64)?;
    if !matches!(
        kind.as_str(),
        "simple" | "exec" | "forking" | "oneshot" | "dbus" | "notify" | "notify-reload" | "idle"
    ) {
        return Err(ProbeIssue::Malformed);
    }
    let pid: u32 = required(values, "MainPID")?;
    let exec: ExecRows = required(values, "ExecStart")?;
    if exec.len() > 8 {
        return Err(ProbeIssue::Oversize);
    }
    for row in &exec {
        bounded_text(&row.0, 4096)?;
        names(&row.1, false)?;
    }
    if pid == 0 || exec.is_empty() {
        return Err(ProbeIssue::Unverified);
    }
    if exec.len() != 1 {
        return Err(ProbeIssue::Ambiguous);
    }
    let row = &exec[0];
    let row_running = row.7 == pid && row.3 != 0 && row.4 != 0 && row.5 == 0 && row.6 == 0;
    let row_unpopulated = (row.3, row.4, row.5, row.6, row.7) == (0, 0, 0, 0, 0);
    if !row_running && !(row_unpopulated && running_main(values, pid)?) {
        return Err(ProbeIssue::Unverified);
    }
    Ok(Running {
        kind,
        pid,
        path: row.0.clone(),
        argv: row.1.clone(),
        ignore_errors: row.2,
    })
}

/// `/usr/bin/gnome-shell`, notify-type, with no argument beyond the unit instance's mode (or the
/// Wayland switches older units pass).
fn shell_command(run: &Running, instance: &str) -> bool {
    let mode = format!("--mode={instance}");
    run.kind == "notify"
        && !run.ignore_errors
        && run.path == GNOME_SHELL
        && run.argv.first().map(String::as_str) == Some(GNOME_SHELL)
        && run.argv[1..]
            .iter()
            .all(|arg| *arg == mode || arg == "--wayland" || arg == "--no-x11")
}

fn bound_to_graphical(unit: &Unit) -> bool {
    unit.part_of
        .iter()
        .chain(&unit.binds)
        .any(|name| name == GRAPHICAL)
}

fn graphical_state(bus: &mut Bus, rows: &UnitRows) -> Result<(bool, u64), ProbeIssue> {
    let active = if rows.iter().any(|r| r.0 == GRAPHICAL) {
        unit(bus, rows, GRAPHICAL)?.active
    } else {
        false
    };
    Ok((active, bus.last_receipt))
}

fn list(bus: &mut Bus, patterns: Vec<&str>) -> Result<UnitRows, ProbeIssue> {
    let rows: UnitRows = bus
        .call(
            DEST,
            ROOT,
            "org.freedesktop.systemd1.Manager",
            "ListUnitsByPatterns",
            &(Vec::<String>::new(), patterns),
        )
        .map_err(bus_issue)?;
    decode_units(rows)
}

fn facts(
    managed: Option<u32>,
    managed_time: u64,
    graphical: bool,
    graphical_time: u64,
) -> ManagerFacts {
    ManagerFacts {
        compositor_managed: Fact::known(managed.is_some(), ObservationSource::Demo, managed_time),
        graphical_target_active: Fact::known(graphical, ObservationSource::Demo, graphical_time),
        compositor_pid: managed,
    }
}

/// GNOME: exactly one active `org.gnome.Shell@{user,wayland}.service` running `gnome-shell`, and
/// exactly one active `gnome-session@*.target` that requires it and is part of (or bound to)
/// `graphical-session.target`.
pub(crate) fn read_gnome(
    stream: UnixStream,
    deadline: &Deadline,
    clock: CallerClock,
) -> Result<ManagerFacts, ProbeIssue> {
    let mut bus = Bus::new(stream, deadline, clock)?;
    let rows = list(&mut bus, vec![SHELL_PATTERN, SESSION_PATTERN, GRAPHICAL])?;
    let is_shell = |name: &str| name.starts_with(SHELL_PREFIX) && name.ends_with(".service");
    let is_session = |name: &str| name.starts_with("gnome-session@") && name.ends_with(".target");
    if rows.iter().any(|row| {
        (is_shell(&row.0) || is_session(&row.0))
            && !matches!(row.3.as_str(), "active" | "inactive" | "failed")
    }) {
        return Err(ProbeIssue::Unverified);
    }
    let listing_time = bus.last_receipt;
    let (graphical, graphical_time) = graphical_state(&mut bus, &rows)?;
    let shells: Vec<_> = rows
        .iter()
        .filter(|r| is_shell(&r.0) && r.3 == "active")
        .collect();
    let sessions: Vec<_> = rows
        .iter()
        .filter(|r| is_session(&r.0) && r.3 == "active")
        .collect();
    if shells.len() > 1 || sessions.len() > 1 {
        return Err(ProbeIssue::Ambiguous);
    }
    let mut pid = None;
    let mut managed_time = listing_time;
    if let (Some(shell_row), Some(session_row)) = (shells.first(), sessions.first()) {
        let shell = unit_with_part_of(&mut bus, &rows, &shell_row.0)?;
        let session = unit_with_part_of(&mut bus, &rows, &session_row.0)?;
        let service = bus.properties(
            DEST,
            shell.path.as_str(),
            "org.freedesktop.systemd1.Service",
        )?;
        let running = running_service(&service)?;
        managed_time = bus.last_receipt;
        let instance = shell
            .id
            .strip_prefix(SHELL_PREFIX)
            .and_then(|s| s.strip_suffix(".service"))
            .ok_or(ProbeIssue::Malformed)?;
        if SHELL_INSTANCES.contains(&instance)
            && shell_command(&running, instance)
            && shell.active
            && session.active
            && session.requires.contains(&shell.id)
            && bound_to_graphical(&session)
        {
            pid = Some(running.pid);
        }
    }
    Ok(facts(pid, managed_time, graphical, graphical_time))
}

/// KDE Plasma: the active `plasma-kwin_wayland.service` running `kwin_wayland_wrapper` (or
/// `kwin_wayland` itself) and part of (or bound to) `graphical-session.target`. The wrapper's
/// child is the compositor; support assembly accepts exactly that one parent-child step.
pub(crate) fn read_kde(
    stream: UnixStream,
    deadline: &Deadline,
    clock: CallerClock,
) -> Result<ManagerFacts, ProbeIssue> {
    let mut bus = Bus::new(stream, deadline, clock)?;
    let rows = list(&mut bus, vec![KWIN_UNIT, GRAPHICAL])?;
    if rows.iter().any(|row| {
        row.0 == KWIN_UNIT && !matches!(row.3.as_str(), "active" | "inactive" | "failed")
    }) {
        return Err(ProbeIssue::Unverified);
    }
    let listing_time = bus.last_receipt;
    let (graphical, graphical_time) = graphical_state(&mut bus, &rows)?;
    let mut pid = None;
    let mut managed_time = listing_time;
    if rows.iter().any(|r| r.0 == KWIN_UNIT && r.3 == "active") {
        let kwin = unit_with_part_of(&mut bus, &rows, KWIN_UNIT)?;
        let service =
            bus.properties(DEST, kwin.path.as_str(), "org.freedesktop.systemd1.Service")?;
        let running = running_service(&service)?;
        managed_time = bus.last_receipt;
        if kwin.active
            && bound_to_graphical(&kwin)
            && !running.ignore_errors
            && matches!(running.path.as_str(), KWIN_WRAPPER | KWIN)
            && running.argv.first() == Some(&running.path)
        {
            pid = Some(running.pid);
        }
    }
    Ok(facts(pid, managed_time, graphical, graphical_time))
}
