//! Parse only selected-job authority from bounded public launchd output.
use std::{collections::BTreeMap, path::Path};

pub(crate) const MAX_LAUNCHD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobObservation {
    Absent,
    Running(u32),
    LoadedStopped,
    Unknown,
}

#[derive(Clone, Copy)]
pub(crate) struct SelectedJob<'a> {
    pub uid: u32,
    pub label: &'a str,
    pub plist: &'a Path,
    pub program: &'a Path,
}

pub(crate) fn job(
    code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    selected: SelectedJob<'_>,
) -> JobObservation {
    let SelectedJob {
        uid,
        label,
        plist,
        program,
    } = selected;
    use JobObservation as J;
    if stdout.len().saturating_add(stderr.len()) > MAX_LAUNCHD_BYTES {
        return J::Unknown;
    }
    let missing = format!("Could not find service \"{label}\" in domain for user gui: {uid}\n");
    if code == Some(113) && stdout.is_empty() && stderr == missing.as_bytes() {
        return J::Absent;
    }
    let text = String::from_utf8_lossy(stdout);
    let header = format!("gui/{uid}/{label} = {{");
    if code != Some(0)
        || text.lines().next().map(str::trim) != Some(header.as_str())
        || text.lines().last().map(str::trim) != Some("}")
    {
        return J::Unknown;
    }
    let mut fields = BTreeMap::new();
    let mut depth = 1usize;
    for (raw, line) in stdout.split(|b| *b == b'\n').zip(text.lines()).skip(1) {
        let line = line.trim();
        if depth == 1 && std::str::from_utf8(raw).is_err() {
            let key = raw.split(|b| *b == b'=').next().unwrap_or(&[]);
            if std::str::from_utf8(key)
                .ok()
                .is_some_and(|key| matches!(key.trim(), "path" | "program" | "pid" | "state"))
            {
                return J::Unknown;
            }
        }
        if depth == 0 {
            return J::Unknown;
        }
        if line == "}" {
            depth -= 1;
            continue;
        }
        let pair = line
            .split_once('=')
            .map(|(key, value)| (key.trim(), value.trim_start_matches('>').trim()));
        if let Some((key, value)) = pair {
            if depth == 1 && matches!(key, "path" | "program" | "pid" | "state") {
                if value.len() > 4096 || fields.insert(key, value).is_some() {
                    return J::Unknown;
                }
                if value != "{" {
                    continue;
                }
            }
            if value == "{" {
                if depth >= 256 {
                    return J::Unknown;
                }
                depth += 1;
            }
        }
        // Unknown scalar fields (including shell strings containing braces) are irrelevant.
    }
    if depth != 0
        || fields.get("path").copied() != plist.to_str()
        || fields.get("program").copied() != program.to_str()
    {
        return J::Unknown;
    }
    match (fields.get("pid"), fields.get("state").copied()) {
        (Some(pid), state) => match pid.parse::<u32>() {
            Ok(0) if state == Some("not running") => J::LoadedStopped,
            Ok(pid) if pid != 0 && state != Some("not running") => J::Running(pid),
            _ => J::Unknown,
        },
        (None, Some("not running")) => J::LoadedStopped,
        _ => J::Unknown,
    }
}

pub(crate) fn disabled(
    code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    label: &str,
) -> Option<bool> {
    if stdout.len().saturating_add(stderr.len()) > MAX_LAUNCHD_BYTES || code != Some(0) {
        return None;
    }
    let text = String::from_utf8_lossy(stdout);
    if text.lines().next()?.trim() != "disabled services = {" || text.lines().last()?.trim() != "}"
    {
        return None;
    }
    let expected = format!("\"{label}\"");
    let mut found = None;
    for line in text
        .lines()
        .skip(1)
        .take(text.lines().count().saturating_sub(2))
    {
        if line.contains('\u{fffd}') && line.contains(label) {
            return None;
        }
        let pair = line
            .trim()
            .split_once("=>")
            .map(|(k, v)| (k.trim(), v.trim()));
        if let Some((_, value)) = pair.filter(|(key, _)| *key == expected) {
            let value = match value {
                "true" => true,
                "false" => false,
                _ => return None,
            };
            if found.replace(value).is_some() {
                return None;
            }
        } else if line.contains(label) || line.trim() == "}" {
            // A malformed selected row or early closing scope cannot prove absence.
            return None;
        }
    }
    Some(found.unwrap_or(false))
}
