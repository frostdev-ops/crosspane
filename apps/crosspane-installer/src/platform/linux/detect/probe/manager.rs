//! Effective systemd properties, never a unit-name/target/environment shortcut to uwsm.
//! The admitted user-manager retains zbus's 128 MiB message-allocation ceiling and uncapped
//! SASL reads. Owned-stream deadlines/cancellation bound time, not allocation.
//! Its endpoint is in the selected user's admitted 0700 runtime. A hostile peer there needs
//! a same-UID actor, outside the 4.12a/4.7c threat model; this endpoint is not root-owned.
use super::*;
use logind::{Bus, bounded_text, bus_issue, property};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Type};

pub type UnitRows = Vec<(
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
)>;
type ExecRows = Vec<(String, Vec<String>, bool, u64, u64, u64, u64, u32, i32, i32)>;
const DEST: &str = "org.freedesktop.systemd1";
const ROOT: &str = "/org/freedesktop/systemd1";
const GRAPHICAL: &str = "graphical-session.target";
fn required<T: TryFrom<OwnedValue> + Type>(
    values: &Properties,
    key: &str,
) -> Result<T, ProbeIssue> {
    property(values, key)?.ok_or(ProbeIssue::Unverified)
}
fn names(values: &[String], nonempty: bool) -> Result<(), ProbeIssue> {
    if values.len() > MAX_SESSIONS {
        return Err(ProbeIssue::Oversize);
    }
    for value in values {
        bounded_text(value, 256)?;
        if nonempty && value.is_empty() {
            return Err(ProbeIssue::Malformed);
        }
    }
    Ok(())
}
/// ListUnitsByPatterns' exact a(ssssssouso) signature. Every returned row is validated.
pub fn decode_units(rows: UnitRows) -> Result<UnitRows, ProbeIssue> {
    if rows.len() > MAX_SESSIONS {
        return Err(ProbeIssue::Oversize);
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut paths = std::collections::HashSet::new();
    for row in &rows {
        for value in [&row.0, &row.1, &row.2, &row.3, &row.4, &row.5, &row.8] {
            bounded_text(value, 512)?;
        }
        for path in [&row.6, &row.9] {
            bounded_text(path.as_str(), 512)?;
        }
        if !matches!(
            row.2.as_str(),
            "stub" | "loaded" | "not-found" | "bad-setting" | "error" | "merged" | "masked"
        ) || !matches!(
            row.3.as_str(),
            "active"
                | "reloading"
                | "inactive"
                | "failed"
                | "activating"
                | "deactivating"
                | "maintenance"
                | "refreshing"
        ) {
            return Err(ProbeIssue::Malformed);
        }
        if row.0.is_empty() || row.6.as_str() == "/" || !ids.insert(&row.0) || !paths.insert(&row.6)
        {
            return Err(ProbeIssue::Malformed);
        }
    }
    Ok(rows)
}
struct Unit {
    id: String,
    active: bool,
    binds: Vec<String>,
    requires: Vec<String>,
    path: OwnedObjectPath,
}
fn unit(bus: &mut Bus, row: &UnitRows, name: &str) -> Result<Unit, ProbeIssue> {
    let row = row
        .iter()
        .find(|row| row.0 == name)
        .ok_or(ProbeIssue::Unverified)?;
    let values = bus.properties(DEST, row.6.as_str(), "org.freedesktop.systemd1.Unit")?;
    let id: String = required(&values, "Id")?;
    let load: String = required(&values, "LoadState")?;
    let active: String = required(&values, "ActiveState")?;
    let binds: Vec<String> = required(&values, "BindsTo")?;
    let requires: Vec<String> = required(&values, "Requires")?;
    for value in [&id, &load, &active] {
        bounded_text(value, 256)?;
    }
    names(&binds, true)?;
    names(&requires, true)?;
    if id != row.0 || load != row.2 || active != row.3 {
        return Err(ProbeIssue::Foreign);
    }
    if load != "loaded" || !matches!(active.as_str(), "active" | "inactive" | "failed") {
        return Err(ProbeIssue::Unverified);
    }
    Ok(Unit {
        id,
        active: active == "active",
        binds,
        requires,
        path: row.6.clone(),
    })
}
fn instance(value: &str) -> Result<String, ProbeIssue> {
    let mut output = Vec::new();
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        output.push(if byte == b'\\' {
            if bytes.next() != Some(b'x') {
                return Err(ProbeIssue::Malformed);
            }
            let digit = |b: Option<u8>| {
                b.and_then(|b| (b as char).to_digit(16))
                    .ok_or(ProbeIssue::Malformed)
            };
            (digit(bytes.next())? * 16 + digit(bytes.next())?) as u8
        } else if byte == b'-' {
            b'/'
        } else {
            byte
        });
    }
    let value = String::from_utf8(output).map_err(|_| ProbeIssue::Malformed)?;
    bounded_text(&value, 256)?;
    if value.is_empty() {
        return Err(ProbeIssue::Malformed);
    }
    Ok(value)
}
/// systemd's running main-process record: same PID as MainPID, started, not exited, no exit status.
fn running_main(values: &Properties, pid: u32) -> Result<bool, ProbeIssue> {
    let main: u32 = required(values, "ExecMainPID")?;
    let start: u64 = required(values, "ExecMainStartTimestamp")?;
    let exit: u64 = required(values, "ExecMainExitTimestamp")?;
    let code: i32 = required(values, "ExecMainCode")?;
    let status: i32 = required(values, "ExecMainStatus")?;
    Ok(main == pid && start != 0 && exit == 0 && code == 0 && status == 0)
}
fn running_uwsm(values: &Properties, id: &str) -> Result<Option<u32>, ProbeIssue> {
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
    // Older systemd fills the ExecStart row while the command runs. systemd 261 leaves it all zero
    // until exit and keeps the running main process in the ExecMain* properties instead.
    let row_running = row.7 == pid && row.3 != 0 && row.4 != 0 && row.5 == 0 && row.6 == 0;
    let row_unpopulated = (row.3, row.4, row.5, row.6, row.7) == (0, 0, 0, 0, 0);
    if !row_running && !(row_unpopulated && running_main(values, pid)?) {
        return Err(ProbeIssue::Unverified);
    }
    let id = id
        .strip_prefix("wayland-wm@")
        .and_then(|s| s.strip_suffix(".service"))
        .ok_or(ProbeIssue::Malformed)?;
    let expected = instance(id)?;
    Ok(exec
        .first()
        .filter(|row| {
            kind == "notify"
                && !row.2
                && row.0 == "/usr/bin/uwsm"
                && row.1.len() >= 5
                && row.1[..4] == ["/usr/bin/uwsm", "aux", "exec", "--"]
                && row.1[4] == expected
        })
        .map(|_| pid))
}
#[derive(Clone, Debug, PartialEq)]
pub struct ManagerFacts {
    pub uwsm_managed: Fact<bool>,
    pub graphical_target_active: Fact<bool>,
    /// Correlate this PID with the selected IPC/Wayland peers in support assembly; not logind.
    pub compositor_pid: Option<u32>,
}
pub(crate) fn read(
    stream: UnixStream,
    deadline: &Deadline,
    clock: CallerClock,
) -> Result<ManagerFacts, ProbeIssue> {
    let mut bus = Bus::new(stream, deadline, clock)?;
    let rows: UnitRows = bus
        .call(
            DEST,
            ROOT,
            "org.freedesktop.systemd1.Manager",
            "ListUnitsByPatterns",
            &(
                Vec::<String>::new(),
                vec![
                    "wayland-wm@*.service",
                    "wayland-session@*.target",
                    GRAPHICAL,
                ],
            ),
        )
        .map_err(bus_issue)?;
    let rows = decode_units(rows)?;
    if rows.iter().any(|row| {
        row.0.starts_with("wayland-wm@")
            && row.0.ends_with(".service")
            && !matches!(row.3.as_str(), "active" | "inactive" | "failed")
    }) {
        return Err(ProbeIssue::Unverified);
    }
    let listing_time = bus.last_receipt;
    let graphical = if rows.iter().any(|r| r.0 == GRAPHICAL) {
        unit(&mut bus, &rows, GRAPHICAL)?.active
    } else {
        false
    };
    let graphical_time = bus.last_receipt;
    let active: Vec<_> = rows
        .iter()
        .filter(|r| r.0.starts_with("wayland-wm@") && r.0.ends_with(".service") && r.3 == "active")
        .collect();
    if active.len() > 1 {
        return Err(ProbeIssue::Ambiguous);
    }
    let mut pid = None;
    let mut management_time = listing_time;
    if let Some(row) = active.first() {
        let wm = unit(&mut bus, &rows, &row.0)?;
        let session = format!("wayland-session@{}.target", &wm.id[11..wm.id.len() - 8]);
        let session = unit(&mut bus, &rows, &session)?;
        let service = bus.properties(DEST, wm.path.as_str(), "org.freedesktop.systemd1.Service")?;
        let candidate = running_uwsm(&service, &wm.id)?;
        management_time = bus.last_receipt;
        if wm.active
            && session.active
            && wm.binds.contains(&session.id)
            && session.binds.iter().any(|v| v == GRAPHICAL)
            && session.requires.contains(&wm.id)
        {
            pid = candidate;
        }
    }
    Ok(ManagerFacts {
        uwsm_managed: Fact::known(pid.is_some(), ObservationSource::Demo, management_time),
        graphical_target_active: Fact::known(graphical, ObservationSource::Demo, graphical_time),
        compositor_pid: pid,
    })
}
pub(crate) fn environment_output(
    output: Result<CommandOutput, NativeError>,
    source: ObservationSource,
    time: u64,
) -> Fact<EffectiveEnvironment> {
    let value = output.map_err(issue).and_then(|output| {
        if output.stdout.len() + output.stderr.len() > MAX_PROBE_BYTES {
            return Err(ProbeIssue::Oversize);
        }
        if output.code != Some(0) {
            return Err(ProbeIssue::Unavailable);
        }
        parse_manager_environment(&output.stdout)
    });
    Fact {
        value,
        source,
        observed_at_ms: time,
    }
}
/// Explicit owned streams produce Demo observations and confer no support authority.
pub fn manager_from_stream(
    stream: UnixStream,
    deadline: &Deadline,
    clock: CallerClock,
) -> Fact<ManagerFacts> {
    let shared = deadline.clone();
    let receipt = clock.clone();
    bounded(
        stream,
        deadline,
        clock,
        ObservationSource::Demo,
        move |stream| read(stream, &shared, receipt),
    )
}
