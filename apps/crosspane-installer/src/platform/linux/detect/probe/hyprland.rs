use super::*;
use std::io::{Read, Write};

#[derive(Clone, Debug, PartialEq)]
pub struct HyprlandFacts {
    pub version: Fact<[u16; 3]>,
    pub pid: u32,
}
/// Reads the version token in the known header; unrelated build metadata is advisory.
pub fn parse_hyprland_version(bytes: &[u8]) -> Result<[u16; 3], ProbeIssue> {
    if bytes.len() > MAX_PROBE_BYTES {
        return Err(ProbeIssue::Oversize);
    }
    let header = bytes
        .split(|b| *b == b'\n')
        .next()
        .ok_or(ProbeIssue::Malformed)?;
    let header = std::str::from_utf8(header).map_err(|_| ProbeIssue::Malformed)?;
    let tail = header
        .strip_prefix("Hyprland version ")
        .or_else(|| header.strip_prefix("Hyprland "))
        .ok_or(ProbeIssue::Malformed)?;
    let token = tail
        .split_ascii_whitespace()
        .next()
        .ok_or(ProbeIssue::Malformed)?;
    let token = token.strip_prefix('v').unwrap_or(token);
    let version = if let Some((version, metadata)) = token.split_once('+') {
        if metadata.is_empty()
            || !metadata
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        {
            return Err(ProbeIssue::Malformed);
        }
        version
    } else {
        token
    };
    let mut parts = version.split('.');
    let mut result = [0; 3];
    for value in &mut result {
        let part = parts.next().ok_or(ProbeIssue::Malformed)?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ProbeIssue::Malformed);
        }
        *value = part.parse().map_err(|_| ProbeIssue::Malformed)?;
    }
    if parts.next().is_some() {
        return Err(ProbeIssue::Malformed);
    }
    Ok(result)
}
pub(crate) fn peer_pid(stream: &UnixStream) -> Result<u32, ProbeIssue> {
    let peer =
        rustix::net::sockopt::socket_peercred(stream).map_err(|_| ProbeIssue::Unavailable)?;
    u32::try_from(peer.pid.as_raw_pid())
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or(ProbeIssue::Foreign)
}
pub(crate) fn read(
    mut stream: UnixStream,
    clock: CallerClock,
) -> Result<HyprlandFacts, ProbeIssue> {
    let pid = peer_pid(&stream)?;
    stream
        .set_nonblocking(false)
        .map_err(|_| ProbeIssue::Unavailable)?;
    stream
        .write_all(b"/version")
        .map_err(|_| ProbeIssue::Unavailable)?;
    let mut bytes = Vec::new();
    stream
        .take(MAX_PROBE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ProbeIssue::Unavailable)?;
    let receipt = clock();
    Ok(HyprlandFacts {
        version: Fact {
            value: parse_hyprland_version(&bytes),
            source: ObservationSource::Demo,
            observed_at_ms: receipt,
        },
        pid,
    })
}
/// Explicit owned streams never establish native support; the worker interrupts only this stream.
pub fn hyprland_from_stream(
    stream: UnixStream,
    deadline: &Deadline,
    clock: CallerClock,
) -> Fact<HyprlandFacts> {
    let receipt = clock.clone();
    bounded(
        stream,
        deadline,
        clock,
        ObservationSource::Demo,
        move |stream| read(stream, receipt),
    )
}
