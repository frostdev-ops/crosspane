//! Turning the Crosspane GNOME Shell extension on and off in the person's own settings.
//!
//! The extension's files are installed with the other desktop files (`payload/desktop.rs`); this
//! module only edits `org.gnome.shell enabled-extensions`, through `/usr/bin/gsettings` on the
//! selected user's session bus. Rules, all fixed here:
//!
//! - The list is parsed strictly. Only extension identifiers made of `A-Za-z0-9._@+-` are
//!   understood; anything else (escapes, other types, other shapes) is "unsafe to rewrite" and
//!   nothing is written.
//! - Enabling appends this extension's identifier to the list as it stands, keeping every other
//!   entry and its order. Disabling removes this identifier only. Neither ever removes or reorders
//!   another entry, and an identifier already in the right state is left alone.
//! - Every change is re-read afterwards. An exit code alone proves nothing.
//! - A failure to enable is a warning for the caller to show, never a failed install: the agent
//!   works without the extension.
//! - `disable-user-extensions` is only read. It is the person's choice and is never changed.
use super::native_io::{
    ChildEnvironment, CommandSpec, Deadline, LinuxNativeIo, NativeError, SupportProof,
};
use std::{collections::BTreeMap, sync::Arc};

/// The extension's identifier: its directory name and `metadata.json` `uuid`.
pub const UUID: &str = "crosspane@frostdev.io";
/// The first Shell major version the extension's `metadata.json` declares support for.
pub const MIN_SHELL: u16 = 48;
/// GNOME loads an extension it has not seen before only when the Shell starts.
pub const NEXT_LOGIN_NOTE: &str = "GNOME loads a newly installed Shell extension at your next log-in, so log out and back in \
     for Crosspane's Shell extension to start.";
pub const SCHEMA: &str = "org.gnome.shell";
pub const ENABLED_KEY: &str = "enabled-extensions";
pub const DISABLED_KEY: &str = "disable-user-extensions";
const GSETTINGS: &str = "/usr/bin/gsettings";
const MAX_ENTRIES: usize = 512;
const MAX_NAME_BYTES: usize = 128;
const MAX_LITERAL_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExtensionError {
    /// Not the shape gsettings prints for a string list (or a boolean).
    #[error("the extension list couldn't be understood")]
    Malformed,
    /// A well-formed list holding an entry that can't be rewritten without risk.
    #[error("the extension list holds an entry that is not safe to rewrite")]
    Unsafe,
    /// The list read back after a change is not the one written.
    #[error("the extension list changed while it was being edited")]
    Changed,
    #[error(transparent)]
    Native(#[from] NativeError),
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._@+-".contains(&b))
}

/// The text `gsettings get` prints for an `as`: `['a@b', 'c@d']`, `@as []` or `[]`.
pub fn parse_enabled(text: &str) -> Result<Vec<String>, ExtensionError> {
    if text.len() > MAX_LITERAL_BYTES || text.chars().any(|c| c.is_control() && c != '\n') {
        return Err(ExtensionError::Malformed);
    }
    let text = text.trim();
    let body = text.strip_prefix("@as").map_or(text, str::trim_start);
    let inner = body
        .strip_prefix('[')
        .and_then(|b| b.strip_suffix(']'))
        .ok_or(ExtensionError::Malformed)?;
    let mut names = Vec::new();
    let mut rest = inner.trim();
    while !rest.is_empty() {
        let quote = rest.chars().next().ok_or(ExtensionError::Malformed)?;
        if quote != '\'' && quote != '"' {
            return Err(ExtensionError::Malformed);
        }
        let end = rest[1..].find(quote).ok_or(ExtensionError::Malformed)? + 1;
        let name = &rest[1..end];
        if !valid_name(name) {
            return Err(ExtensionError::Unsafe);
        }
        names.push(name.to_owned());
        if names.len() > MAX_ENTRIES {
            return Err(ExtensionError::Unsafe);
        }
        rest = rest[end + 1..].trim_start();
        if rest.is_empty() {
            break;
        }
        rest = rest
            .strip_prefix(',')
            .ok_or(ExtensionError::Malformed)?
            .trim_start();
    }
    Ok(names)
}

/// The literal `gsettings set` is given: `['a@b', 'c@d']`, `[]` when empty.
pub fn render(list: &[String]) -> Result<String, ExtensionError> {
    if list.len() > MAX_ENTRIES || list.iter().any(|name| !valid_name(name)) {
        return Err(ExtensionError::Unsafe);
    }
    let items: Vec<String> = list.iter().map(|name| format!("'{name}'")).collect();
    Ok(format!("[{}]", items.join(", ")))
}

/// Whether `value` is exactly a literal `render` could have produced. The native command
/// allowlist admits nothing else as the value of `gsettings set org.gnome.shell
/// enabled-extensions`.
pub(crate) fn is_list_literal(value: &str) -> bool {
    value.len() <= MAX_LITERAL_BYTES
        && value.starts_with('[')
        && value.ends_with(']')
        && parse_enabled(value).is_ok_and(|list| render(&list).is_ok_and(|text| text == value))
}

fn parse_bool(text: &str) -> Result<bool, ExtensionError> {
    match text.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ExtensionError::Malformed),
    }
}

/// The list with this extension added at the end, or as it was when it is already there.
pub fn with_extension(list: &[String]) -> Vec<String> {
    let mut result = list.to_vec();
    if !result.iter().any(|name| name == UUID) {
        result.push(UUID.to_owned());
    }
    result
}

/// The list without this extension; every other entry stays, in order.
pub fn without_extension(list: &[String]) -> Vec<String> {
    list.iter().filter(|name| *name != UUID).cloned().collect()
}

/// What the settings say about the extension right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// This extension's identifier is in `enabled-extensions`.
    pub enabled: bool,
    /// The person has GNOME's switch for all user extensions off; nothing loads then.
    pub user_extensions_disabled: bool,
}

/// What an enable or disable did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Already in the wanted state; nothing was written.
    Unchanged,
    /// Written, and read back as expected.
    Written,
}

/// `gsettings` on the selected session bus, for the one key this module owns.
pub struct GnomeSettings {
    io: Arc<LinuxNativeIo>,
    environment: ChildEnvironment,
}

impl std::fmt::Debug for GnomeSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GnomeSettings")
    }
}

impl GnomeSettings {
    /// `session` is the selected environment; only its session-bus address is used, and only
    /// after the same bounded admission the manager commands get.
    pub fn new(
        io: Arc<LinuxNativeIo>,
        session: &BTreeMap<String, String>,
        deadline: &Deadline,
    ) -> Result<Self, ExtensionError> {
        let bus: BTreeMap<String, String> = session
            .iter()
            .filter(|(key, _)| key.as_str() == "DBUS_SESSION_BUS_ADDRESS")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if bus.is_empty() {
            return Err(NativeError::Unavailable.into());
        }
        let environment = io.session_bus_environment(bus, deadline)?;
        Ok(Self { io, environment })
    }

    fn command(&self, argv: &[&str]) -> Result<CommandSpec, ExtensionError> {
        Ok(CommandSpec::new(
            GSETTINGS.into(),
            argv.iter().map(|s| (*s).to_owned()).collect(),
            self.environment.clone(),
            MAX_LITERAL_BYTES,
        )?)
    }

    fn get(&self, key: &str, deadline: &Deadline) -> Result<String, ExtensionError> {
        let output = self
            .io
            .run(&self.command(&["get", SCHEMA, key])?, deadline)?;
        if output.code != Some(0) {
            return Err(NativeError::Unavailable.into());
        }
        String::from_utf8(output.stdout).map_err(|_| ExtensionError::Malformed)
    }

    fn list(&self, deadline: &Deadline) -> Result<Vec<String>, ExtensionError> {
        parse_enabled(&self.get(ENABLED_KEY, deadline)?)
    }

    /// Read only.
    pub fn read(&self, deadline: &Deadline) -> Result<Reading, ExtensionError> {
        let list = self.list(deadline)?;
        let disabled = parse_bool(&self.get(DISABLED_KEY, deadline)?)?;
        Ok(Reading {
            enabled: list.iter().any(|name| name == UUID),
            user_extensions_disabled: disabled,
        })
    }

    fn write(
        &self,
        proof: &SupportProof,
        wanted: &[String],
        deadline: &Deadline,
    ) -> Result<(), ExtensionError> {
        let literal = render(wanted)?;
        let spec = self.command(&["set", SCHEMA, ENABLED_KEY, &literal])?;
        let output = self.io.settings_mutation(proof, &spec, deadline)?;
        if output.code != Some(0) {
            return Err(NativeError::Unavailable.into());
        }
        // Verified by reading, not by the exit code.
        if self.list(deadline)? != wanted {
            return Err(ExtensionError::Changed);
        }
        Ok(())
    }

    /// Append this extension's identifier to `enabled-extensions`, keeping everything else.
    pub fn enable(
        &self,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> Result<Change, ExtensionError> {
        let list = self.list(deadline)?;
        if list.iter().any(|name| name == UUID) {
            return Ok(Change::Unchanged);
        }
        self.write(proof, &with_extension(&list), deadline)?;
        Ok(Change::Written)
    }

    /// Remove this extension's identifier from `enabled-extensions`, keeping everything else.
    pub fn disable(
        &self,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> Result<Change, ExtensionError> {
        let list = self.list(deadline)?;
        if !list.iter().any(|name| name == UUID) {
            return Ok(Change::Unchanged);
        }
        self.write(proof, &without_extension(&list), deadline)?;
        Ok(Change::Written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_what_gsettings_prints() {
        assert_eq!(parse_enabled("@as []\n"), Ok(vec![]));
        assert_eq!(parse_enabled("[]"), Ok(vec![]));
        assert_eq!(
            parse_enabled("['a@b.org', \"c-d@e\"]\n"),
            Ok(names(&["a@b.org", "c-d@e"]))
        );
        assert_eq!(parse_enabled("['a@b',]"), Ok(names(&["a@b"])));
    }

    #[test]
    fn anything_else_is_malformed_or_unsafe_and_never_rewritten() {
        for text in [
            "",
            "['a'",
            "'a'",
            "['a' 'b']",
            "[1]",
            "['a',, 'b']",
            "{'a'}",
        ] {
            assert_eq!(
                parse_enabled(text),
                Err(ExtensionError::Malformed),
                "{text:?}"
            );
        }
        for text in ["['a b']", "['a\\'b']", "['']", "['a/b']", "['$(x)']"] {
            assert_eq!(parse_enabled(text), Err(ExtensionError::Unsafe), "{text:?}");
        }
        assert_eq!(
            render(&names(&["a b"])),
            Err(ExtensionError::Unsafe),
            "render checks too"
        );
    }

    #[test]
    fn enabling_appends_ours_and_keeps_every_other_entry_in_order() {
        let before = names(&["z@z", "a@a"]);
        let after = with_extension(&before);
        assert_eq!(after, names(&["z@z", "a@a", UUID]));
        assert_eq!(with_extension(&after), after, "idempotent");
        assert_eq!(render(&after).unwrap(), format!("['z@z', 'a@a', '{UUID}']"));
        assert_eq!(render(&[]).unwrap(), "[]");
    }

    #[test]
    fn disabling_removes_only_ours() {
        let list = names(&["z@z", UUID, "a@a", UUID]);
        assert_eq!(without_extension(&list), names(&["z@z", "a@a"]));
        assert_eq!(without_extension(&names(&["z@z"])), names(&["z@z"]));
    }

    #[test]
    fn only_rendered_lists_are_admitted_as_the_set_value() {
        assert!(is_list_literal("[]"));
        assert!(is_list_literal(&format!("['a@b', '{UUID}']")));
        for value in [
            "@as []",
            "['a@b',]",
            "[\"a@b\"]",
            "['a b']",
            "['a@b'] ; rm",
            "['a@b']\n",
            "['a@b','c@d']",
        ] {
            assert!(!is_list_literal(value), "{value:?}");
        }
    }

    #[test]
    fn the_extension_identifier_is_a_valid_entry_and_matches_the_packaged_metadata() {
        assert!(valid_name(UUID));
        let metadata = include_str!(
            "../../../../../packaging/gnome-shell-extension/crosspane@frostdev.io/metadata.json"
        );
        assert!(metadata.contains(&format!("\"uuid\": \"{UUID}\"")));
        let versions: Vec<u16> = metadata
            .split("\"shell-version\"")
            .nth(1)
            .and_then(|tail| tail.split('[').nth(1))
            .and_then(|tail| tail.split(']').next())
            .map(|list| {
                list.split(',')
                    .filter_map(|v| v.trim().trim_matches('"').parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(versions.iter().min().copied(), Some(MIN_SHELL));
    }
}
