use super::{Architecture, EffectiveEnvironment, OsFamily, ProbeIssue};
use std::collections::BTreeMap;

pub const MAX_PROBE_BYTES: usize = 1024 * 1024;
pub const MAX_SESSIONS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionSelection {
    Pid,
    Environment,
    Display,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCandidate {
    pub id: String,
    pub path: String,
    pub kind: Option<String>,
    pub uid: Option<u32>,
    pub seat: Option<String>,
    pub active: Option<bool>,
    pub locked_hint: Option<bool>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedSession {
    pub selection: SessionSelection,
    pub session: SessionCandidate,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplaySessions {
    pub display: String,
    pub sessions: Vec<SessionCandidate>,
}
/// Pure acquisition seam. The native implementation reads only the first necessary step;
/// PID means THIS process, never the compositor's PID. None is exact logind absence only.
pub trait SessionLookup {
    fn own_session(&mut self) -> Result<Option<SessionCandidate>, ProbeIssue>;
    fn named_session(&mut self, id: &str) -> Result<Option<SessionCandidate>, ProbeIssue>;
    fn display_sessions(&mut self, uid: u32) -> Result<DisplaySessions, ProbeIssue>;
}
fn graphical(candidate: &SessionCandidate, uid: u32) -> bool {
    candidate.uid == Some(uid) && matches!(candidate.kind.as_deref(), Some("wayland" | "x11"))
}
pub fn choose_session(
    reader: &mut impl SessionLookup,
    uid: u32,
    environment_id: Option<&str>,
) -> Result<Option<SelectedSession>, ProbeIssue> {
    if let Some(session) = reader.own_session()?
        && graphical(&session, uid)
    {
        return Ok(Some(SelectedSession {
            selection: SessionSelection::Pid,
            session,
        }));
    }
    if let Some(id) = environment_id.filter(|id| !id.is_empty()) {
        text(id, 64)?;
        if let Some(session) = reader.named_session(id)?
            && graphical(&session, uid)
        {
            return Ok(Some(SelectedSession {
                selection: SessionSelection::Environment,
                session,
            }));
        }
    }
    let display = reader.display_sessions(uid)?;
    if display.sessions.len() > MAX_SESSIONS {
        return Err(ProbeIssue::Oversize);
    }
    if display.display == "/" {
        return Ok(None);
    }
    text(&display.display, 512)?;
    if display
        .sessions
        .iter()
        .any(|s| s.kind.is_none() || s.uid.is_none() || s.seat.is_none())
    {
        return Err(ProbeIssue::Unverified);
    }
    let mut candidates = display
        .sessions
        .into_iter()
        .filter(|s| graphical(s, uid) && s.seat.as_deref().is_some_and(|s| !s.is_empty()));
    let first = candidates.next();
    if candidates.next().is_some() {
        return Err(ProbeIssue::Ambiguous);
    }
    Ok(first
        .filter(|s| s.path == display.display)
        .map(|session| SelectedSession {
            selection: SessionSelection::Display,
            session,
        }))
}
/// Only the producer's exact absence reply permits fallback; all other bus errors stay pending.
pub fn lookup_reply(
    reply: zbus::Result<zbus::zvariant::OwnedObjectPath>,
    step: SessionSelection,
) -> Result<Option<String>, ProbeIssue> {
    match reply {
        Ok(path) => {
            text(path.as_str(), 512)?;
            Ok(Some(path.into_inner().to_string()))
        }
        Err(zbus::Error::MethodError(name, _, _))
            if matches!(
                (step, name.as_str()),
                (
                    SessionSelection::Pid,
                    "org.freedesktop.login1.NoSessionForPID"
                ) | (
                    SessionSelection::Environment,
                    "org.freedesktop.login1.NoSuchSession"
                )
            ) =>
        {
            Ok(None)
        }
        Err(_) => Err(ProbeIssue::Unavailable),
    }
}
fn text(value: &str, limit: usize) -> Result<(), ProbeIssue> {
    if value.is_empty() || value.len() > limit || value.chars().any(char::is_control) {
        Err(ProbeIssue::Malformed)
    } else {
        Ok(())
    }
}
// os-release(5): one shell word, no expansion or concatenated quoted fragments.
fn shell_value(value: &str) -> Result<String, ProbeIssue> {
    let mut chars = value.chars().peekable();
    let quote = chars.peek().copied().filter(|c| matches!(c, '\'' | '"'));
    if quote.is_some() {
        chars.next();
    }
    let mut decoded = String::new();
    while let Some(c) = chars.next() {
        if Some(c) == quote {
            if chars.next().is_some() {
                return Err(ProbeIssue::Malformed);
            }
            return Ok(decoded);
        }
        match c {
            '\\' if quote != Some('\'') => {
                let escaped = chars.next().ok_or(ProbeIssue::Malformed)?;
                if quote == Some('"') && !matches!(escaped, '$' | '`' | '"' | '\\') {
                    decoded.push('\\');
                }
                decoded.push(escaped);
            }
            '$' | '`' if quote != Some('\'') => return Err(ProbeIssue::Malformed),
            c if quote.is_none() && (c.is_whitespace() || "\"';&|<>()".contains(c)) => {
                return Err(ProbeIssue::Malformed);
            }
            _ => decoded.push(c),
        }
    }
    if quote.is_some() {
        Err(ProbeIssue::Malformed)
    } else {
        Ok(decoded)
    }
}
fn valid_key(key: &str) -> bool {
    key.len() <= 128
        && key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn assignments(bytes: &[u8]) -> Result<BTreeMap<String, String>, ProbeIssue> {
    if bytes.len() > MAX_PROBE_BYTES {
        return Err(ProbeIssue::Oversize);
    }
    let mut values = BTreeMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let Some(split) = line.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let key = &line[..split];
        if !matches!(key, b"ID" | b"ID_LIKE") {
            continue;
        }
        let key = std::str::from_utf8(key).map_err(|_| ProbeIssue::Malformed)?;
        let value = std::str::from_utf8(&line[split + 1..]).map_err(|_| ProbeIssue::Malformed)?;
        let value = shell_value(value)?;
        if value.len() > MAX_VALUE_BYTES
            || value.chars().any(char::is_control)
            || values.insert(key.into(), value).is_some()
        {
            return Err(ProbeIssue::Malformed);
        }
    }
    Ok(values)
}
const MAX_VALUE_BYTES: usize = 4096;
/// `systemctl --user show-environment` lines considered; bytes stay bounded by MAX_PROBE_BYTES.
pub const MAX_ENVIRONMENT_LINES: usize = 16 * 1024;
/// The only manager variables detection reads. Each must decode exactly or the read is malformed.
const STRICT_ENVIRONMENT: [&str; 4] = [
    "XDG_RUNTIME_DIR",
    "WAYLAND_DISPLAY",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "XDG_SESSION_ID",
];
/// systemd's POSIX `$'…'` form (escape.c `shell_maybe_quote`/`cescape_char`): `\a \b \f \n \r
/// \t \v \\ \' \"` and `\xHH`. Any other escape, a missing or early closing quote, or
/// non-UTF-8 result is refused.
fn c_quoted(value: &str) -> Result<String, ProbeIssue> {
    let body = value.strip_prefix("$'").ok_or(ProbeIssue::Malformed)?;
    let mut decoded = Vec::with_capacity(body.len());
    let mut chars = body.chars();
    loop {
        match chars.next().ok_or(ProbeIssue::Malformed)? {
            '\'' => {
                if chars.next().is_some() {
                    return Err(ProbeIssue::Malformed);
                }
                return String::from_utf8(decoded).map_err(|_| ProbeIssue::Malformed);
            }
            '\\' => {
                let byte = match chars.next().ok_or(ProbeIssue::Malformed)? {
                    'a' => 0x07,
                    'b' => 0x08,
                    'f' => 0x0c,
                    'n' => b'\n',
                    'r' => b'\r',
                    't' => b'\t',
                    'v' => 0x0b,
                    '\\' => b'\\',
                    '\'' => b'\'',
                    '"' => b'"',
                    'x' => {
                        let mut digit = || {
                            chars
                                .next()
                                .and_then(|c| c.to_digit(16))
                                .ok_or(ProbeIssue::Malformed)
                        };
                        (digit()? * 16 + digit()?) as u8
                    }
                    _ => return Err(ProbeIssue::Malformed),
                };
                decoded.push(byte);
            }
            c => decoded.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
}
fn manager_value(value: &[u8]) -> Result<String, ProbeIssue> {
    let value = std::str::from_utf8(value).map_err(|_| ProbeIssue::Malformed)?;
    let decoded = if value.starts_with("$'") {
        c_quoted(value)?
    } else {
        shell_value(value)?
    };
    if decoded.len() > MAX_VALUE_BYTES || decoded.chars().any(char::is_control) {
        return Err(ProbeIssue::Malformed);
    }
    Ok(decoded)
}
/// One `KEY=value` per line, as systemd prints it. Only STRICT_ENVIRONMENT values must decode;
/// an unrelated line that can't be decoded is skipped, never repaired or returned.
fn manager_assignments(bytes: &[u8]) -> Result<BTreeMap<String, String>, ProbeIssue> {
    if bytes.len() > MAX_PROBE_BYTES {
        return Err(ProbeIssue::Oversize);
    }
    let mut values = BTreeMap::new();
    for (index, line) in bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if index >= MAX_ENVIRONMENT_LINES {
            return Err(ProbeIssue::Oversize);
        }
        let Some(split) = line.iter().position(|b| *b == b'=') else {
            continue;
        };
        // Strict keys are ASCII, so a non-UTF-8 or invalid key can never be one of them.
        let Some(key) = std::str::from_utf8(&line[..split])
            .ok()
            .filter(|key| valid_key(key))
        else {
            continue;
        };
        if !STRICT_ENVIRONMENT.contains(&key) {
            continue;
        }
        match manager_value(&line[split + 1..]) {
            Ok(value) => {
                if values.insert(key.to_owned(), value).is_some() {
                    return Err(ProbeIssue::Malformed);
                }
            }
            Err(error) if STRICT_ENVIRONMENT.contains(&key) => return Err(error),
            Err(_) => {}
        }
    }
    Ok(values)
}
pub fn parse_os_release(bytes: &[u8]) -> Result<OsFamily, ProbeIssue> {
    let values = assignments(bytes)?;
    let id = values.get("ID").ok_or(ProbeIssue::Malformed)?;
    let alike = values.get("ID_LIKE").map(String::as_str).unwrap_or("");
    for identifier in std::iter::once(id.as_str()).chain(alike.split(' ').filter(|v| !v.is_empty()))
    {
        text(identifier, 64)?;
        if !identifier
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        {
            return Err(ProbeIssue::Malformed);
        }
    }
    Ok(if id == "arch" || alike.split(' ').any(|v| v == "arch") {
        OsFamily::Arch
    } else {
        OsFamily::Other(id.clone())
    })
}
pub fn parse_architecture(value: &str) -> Result<Architecture, ProbeIssue> {
    text(value, 32)?;
    Ok(match value {
        "x86_64" => Architecture::X86_64,
        "aarch64" => Architecture::Aarch64,
        other => Architecture::Other(other.into()),
    })
}
pub fn parse_manager_environment(bytes: &[u8]) -> Result<EffectiveEnvironment, ProbeIssue> {
    let values = manager_assignments(bytes)?;
    let required = |key| values.get(key).cloned().ok_or(ProbeIssue::Missing);
    let runtime_dir = required("XDG_RUNTIME_DIR")?;
    let wayland_display = required("WAYLAND_DISPLAY")?;
    let signature = required("HYPRLAND_INSTANCE_SIGNATURE")?;
    text(&runtime_dir, 4096)?;
    if !runtime_dir.starts_with('/')
        || runtime_dir
            .split('/')
            .skip(1)
            .any(|s| s.is_empty() || s == "." || s == "..")
        || wayland_display.is_empty()
        || matches!(wayland_display.as_str(), "." | "..")
        || wayland_display.len() > 128
        || !wayland_display
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
        || signature.is_empty()
        || matches!(signature.as_str(), "." | "..")
        || signature.len() > 256
        || !signature
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
    {
        return Err(ProbeIssue::Malformed);
    }
    if let Some(id) = values.get("XDG_SESSION_ID") {
        text(id, 64)?;
    }
    Ok(EffectiveEnvironment {
        runtime_dir: runtime_dir.into(),
        wayland_display,
        hyprland_instance_signature: signature,
        session_id: values.get("XDG_SESSION_ID").cloned(),
    })
}
