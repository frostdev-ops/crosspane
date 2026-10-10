//! GNOME Shell's version from the `ShellVersion` property of `org.gnome.Shell`, over the selected
//! user's admitted session bus. Information for the person and for the extension's own
//! compatibility check; it is never part of support authority and sets no floor.
use super::*;
use logind::{Bus, bounded_text, property};

const SHELL_NAME: &str = "org.gnome.Shell";
const SHELL_PATH: &str = "/org/gnome/Shell";

/// `50.4`, `49.2.1`, `50.rc`, `50.beta`: the leading numeric components, the rest zero.
pub fn parse_shell_version(text: &str) -> Result<[u16; 3], ProbeIssue> {
    bounded_text(text, 64)?;
    let mut parts = text.split('.');
    let major = parts
        .next()
        .and_then(|part| part.parse::<u16>().ok())
        .ok_or(ProbeIssue::Malformed)?;
    let mut version = [major, 0, 0];
    for slot in &mut version[1..] {
        match parts.next().map(str::parse::<u16>) {
            Some(Ok(value)) => *slot = value,
            _ => break,
        }
    }
    Ok(version)
}

pub(crate) fn read(
    stream: UnixStream,
    deadline: &Deadline,
    clock: CallerClock,
) -> Result<[u16; 3], ProbeIssue> {
    let mut bus = Bus::new(stream, deadline, clock)?;
    let values = bus.properties(SHELL_NAME, SHELL_PATH, SHELL_NAME)?;
    let text: String = property(&values, "ShellVersion")?.ok_or(ProbeIssue::Unverified)?;
    parse_shell_version(&text)
}

/// Explicit owned streams produce Demo observations and confer no support authority.
pub fn shell_version_from_stream(
    stream: UnixStream,
    deadline: &Deadline,
    clock: CallerClock,
) -> Fact<[u16; 3]> {
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
