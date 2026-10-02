//! The home bind: a Hyprland keybind that exists only while the controller is home and runs a
//! shell command (`crosspanectl release`) when the node's release chord is pressed (WP-2.43 §2.9).
//!
//! This is a free-standing module, **not** an implementation of `GlobalHotkeys`: a keybind can't
//! give that trait's physical-keyboard authority (a virtual keyboard presses it exactly like a
//! physical one), so the engine instead guarantees that no Crosspane-injected key can reach the
//! bind while it exists (the provenance invariant, §2.4). Hyprland's side is plain Lua over IPC:
//!
//! ```lua
//! hl.bind("CTRL + SHIFT + ALT + Escape", hl.dsp.exec_cmd([==[<command>]==]),
//!         { submap_universal = true, dont_inhibit = true, description = "crosspane-home-release" })
//! ```
//!
//! - `submap_universal` makes it fire whatever submap is active; `dont_inhibit` keeps it alive
//!   while a window holds a shortcuts inhibitor, so no window can trap the user.
//! - It fires on **press only**: there is no `release` bind and no timing.
//! - The description is the only identification Hyprland gives us (a Lua dispatcher lists as
//!   `__lua`), so a bind is *ours* when it uses the chord and carries [`DESCRIPTION`].
//!
//! # Ownership (A2, B5)
//!
//! `hl.unbind` removes **every** bind that spells the same keys, the owner's included. The module
//! therefore reads `hyprctl binds -j` before every change and never unbinds a chord that a foreign
//! bind uses. A bind "uses the chord" when its `modmask` equals the chord's and its key resolves to
//! the same keysym (case and aliases folded, as Hyprland resolves them); the submap doesn't
//! matter, because `hl.unbind` ignores it too. Binds that carry our description at some other chord
//! are not on the chord and are ignored: this module only ever changes its own chord.
//!
//! | on the chord              | `install`                  | `remove`                         |
//! |---------------------------|----------------------------|----------------------------------|
//! | nothing                   | bind, verify               | `Ok` (absent)                    |
//! | ours only                 | clear the stale copy, bind | unbind, verify absent            |
//! | foreign only              | `Err` (collision)          | `Ok`, foreign bind left alone    |
//! | ours and foreign          | `Err` (collision)          | `Err`; unbind would take both    |
//! | read fails, or an entry is malformed (no string `description` etc.) | `Err` | `Err` (ownership uncertain) |
//!
//! A config reload drops runtime binds, so after the `Err` on "ours and foreign" the caller's next
//! retry finds ours gone. **Residual (accepted):** a foreign bind added between the read and the
//! `hl.unbind` of the "ours only" case is removed with ours. The window is milliseconds and the
//! loss is a foreign *runtime* bind, which a config reload restores.
//!
//! Every request goes through [`HyprIpc`]: short-lived, with its timeout. The module never retries
//! (the agent retries on its housekeeping pass) and never rolls back a failed `install`: the engine
//! always follows a failed install with a removal.

use crosspane_platform::{Chord, PlatformError};
use crosspane_types::hid::HidUsage;
use xkbcommon::xkb;

use super::ipc::HyprIpc;

/// The fixed, unique description that identifies the home bind in `hyprctl binds -j`.
pub const DESCRIPTION: &str = "crosspane-home-release";

/// Hyprland's modifier masks (`src/devices/IKeyboard.hpp`).
const MOD_SHIFT: u32 = 1;
const MOD_CTRL: u32 = 4;
const MOD_ALT: u32 = 8;
const MOD_SUPER: u32 = 64;

/// The keybind that exists only while the controller is home (WP-2.43 §2.9).
#[derive(Clone, Debug)]
pub struct HomeBind {
    core: Core<HyprIpc>,
}

impl HomeBind {
    /// `command` is run by `/bin/sh -c` when the chord is pressed. `Unsupported` if the chord
    /// has no Hyprland spelling or `command` cannot be quoted safely.
    ///
    /// Safe quoting means: not empty, no control characters (the command travels in a one-line
    /// IPC request), no `]==]` (it would end the Lua string it is embedded in) and balanced shell
    /// quotes, so `sh -c` can't fail on syntax when the chord is pressed. Callers quote their own
    /// arguments (`'/path with space/crosspanectl'`); a `'` inside such an argument is refused.
    pub fn new(ipc: HyprIpc, chord: &Chord, command: &str) -> Result<HomeBind, PlatformError> {
        Ok(HomeBind {
            core: Core::new(ipc, chord, command)?,
        })
    }

    /// Install and verify (§2.9). `Backend(..)` if another bind holds the chord.
    pub fn install(&self) -> Result<(), PlatformError> {
        self.core.install()
    }

    /// Remove and verify absent; idempotent.
    pub fn remove(&self) -> Result<(), PlatformError> {
        self.core.remove()
    }

    /// Whether `binds -j` lists exactly our bind.
    pub fn installed(&self) -> Result<bool, PlatformError> {
        self.core.installed()
    }

    /// The Hyprland key string, e.g. `CTRL + SHIFT + ALT + Escape`, for notices.
    pub fn keys(&self) -> &str {
        &self.core.spelling.keys
    }
}

/// What the module needs from the compositor. [`HyprIpc`] implements it; the unit tests use a fake
/// that models Hyprland's bind table.
trait Compositor {
    /// `hyprctl binds -j`, parsed.
    fn binds(&self) -> Result<serde_json::Value, PlatformError>;
    /// Run a Lua chunk in the compositor; `Ok` only if it ran.
    fn run_lua(&self, lua: &str) -> Result<(), PlatformError>;
}

impl Compositor for HyprIpc {
    fn binds(&self) -> Result<serde_json::Value, PlatformError> {
        self.json("binds")
    }

    fn run_lua(&self, lua: &str) -> Result<(), PlatformError> {
        self.eval(lua)
    }
}

#[derive(Clone, Debug)]
struct Core<C> {
    ipc: C,
    spelling: Spelling,
    command: String,
}

impl<C: Compositor> Core<C> {
    fn new(ipc: C, chord: &Chord, command: &str) -> Result<Core<C>, PlatformError> {
        let spelling = Spelling::of(chord)?;
        if !command_is_quotable(command) {
            return Err(PlatformError::Unsupported(
                "home bind command can't be quoted safely",
            ));
        }
        Ok(Core {
            ipc,
            spelling,
            command: command.to_owned(),
        })
    }

    fn install(&self) -> Result<(), PlatformError> {
        let owned = self.read()?;
        let clear_stale = match install_plan(&owned) {
            InstallPlan::Refuse => {
                return Err(PlatformError::Backend(format!(
                    "home bind: another binding already uses {}",
                    self.spelling.keys
                )));
            }
            InstallPlan::Bind { clear_stale } => clear_stale,
        };
        if clear_stale {
            self.ipc.run_lua(&unbind_lua(&self.spelling.keys))?;
        }
        self.ipc
            .run_lua(&bind_lua(&self.spelling.keys, &self.command))?;
        if self.read()?.is_exactly_ours() {
            Ok(())
        } else {
            Err(PlatformError::Backend(format!(
                "home bind: {} is not listed as exactly our binding after install",
                self.spelling.keys
            )))
        }
    }

    fn remove(&self) -> Result<(), PlatformError> {
        match remove_plan(&self.read()?) {
            RemovePlan::Absent => Ok(()),
            RemovePlan::ForeignOnly => {
                tracing::warn!("home bind: only a foreign binding uses the chord; leaving it");
                Ok(())
            }
            RemovePlan::Refuse => Err(PlatformError::Backend(format!(
                "home bind: another binding shares {} with ours; not removing it \
                 (reload the Hyprland config)",
                self.spelling.keys
            ))),
            RemovePlan::Unbind => {
                self.ipc.run_lua(&unbind_lua(&self.spelling.keys))?;
                if self.read()?.ours == 0 {
                    Ok(())
                } else {
                    Err(PlatformError::Backend(format!(
                        "home bind: {} is still bound after unbind",
                        self.spelling.keys
                    )))
                }
            }
        }
    }

    fn installed(&self) -> Result<bool, PlatformError> {
        Ok(self.read()?.is_exactly_ours())
    }

    /// One bounded `binds -j` request, classified against the chord.
    fn read(&self) -> Result<Ownership, PlatformError> {
        let listed = parse_binds(&self.ipc.binds()?)?;
        Ok(Ownership::classify(&listed, &self.spelling))
    }
}

/// A chord in Hyprland's terms.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Spelling {
    /// `CTRL + SHIFT + ALT + Escape`: what `hl.bind` and `hl.unbind` take.
    keys: String,
    /// The key part of `keys`, an xkb keysym name.
    key_name: &'static str,
    /// Hyprland's modifier mask for the modifiers.
    modmask: u32,
}

impl Spelling {
    fn of(chord: &Chord) -> Result<Spelling, PlatformError> {
        let mut modmask = 0;
        for usage in &chord.modifiers {
            modmask |= modifier_mask(*usage).ok_or(PlatformError::Unsupported(
                "home bind chord modifier has no Hyprland spelling",
            ))?;
        }
        let key_name = key_name(chord.key).ok_or(PlatformError::Unsupported(
            "home bind chord key has no Hyprland spelling",
        ))?;
        let mut parts: Vec<&str> = [
            (MOD_CTRL, "CTRL"),
            (MOD_SHIFT, "SHIFT"),
            (MOD_ALT, "ALT"),
            (MOD_SUPER, "SUPER"),
        ]
        .into_iter()
        .filter(|(mask, _)| modmask & mask != 0)
        .map(|(_, name)| name)
        .collect();
        parts.push(key_name);
        Ok(Spelling {
            keys: parts.join(" + "),
            key_name,
            modmask,
        })
    }

    /// Whether a listed bind is on this chord.
    fn is_on_chord(&self, bind: &ListedBind) -> bool {
        bind.modmask == self.modmask && same_key(&bind.key, self.key_name)
    }
}

/// HID modifier usages (0xE0..=0xE7) to Hyprland's mask; left and right fold together.
fn modifier_mask(usage: HidUsage) -> Option<u32> {
    if usage.page != HidUsage::PAGE_KEYBOARD {
        return None;
    }
    match usage.id {
        0xE0 | 0xE4 => Some(MOD_CTRL),
        0xE1 | 0xE5 => Some(MOD_SHIFT),
        0xE2 | 0xE6 => Some(MOD_ALT),
        0xE3 | 0xE7 => Some(MOD_SUPER),
        _ => None,
    }
}

/// The xkb keysym name of a key on the Keyboard page, from the table the design fixes (§2.9):
/// letters, digits, F1-F12, Escape, Pause, Print, Insert, Delete, Home, End, Page_Up, Page_Down,
/// space, Tab, BackSpace, Return.
fn key_name(usage: HidUsage) -> Option<&'static str> {
    const LETTERS: [&str; 26] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t", "u", "v", "w", "x", "y", "z",
    ];
    const DIGITS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];
    const FUNCTION: [&str; 12] = [
        "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
    ];
    if usage.page != HidUsage::PAGE_KEYBOARD {
        return None;
    }
    match usage.id {
        id @ 0x04..=0x1D => LETTERS.get(usize::from(id - 0x04)).copied(),
        id @ 0x1E..=0x27 => DIGITS.get(usize::from(id - 0x1E)).copied(),
        0x28 => Some("Return"),
        0x29 => Some("Escape"),
        0x2A => Some("BackSpace"),
        0x2B => Some("Tab"),
        0x2C => Some("space"),
        id @ 0x3A..=0x45 => FUNCTION.get(usize::from(id - 0x3A)).copied(),
        0x46 => Some("Print"),
        0x48 => Some("Pause"),
        0x49 => Some("Insert"),
        0x4A => Some("Home"),
        0x4B => Some("Page_Up"),
        0x4C => Some("Delete"),
        0x4D => Some("End"),
        0x4E => Some("Page_Down"),
        _ => None,
    }
}

/// Whether two key names are the same key to Hyprland, which resolves them with
/// `xkb_keysym_from_name(.., XKB_KEYSYM_CASE_INSENSITIVE)`: `escape` is `Escape`, `Prior` is
/// `Page_Up`. Names that aren't keysyms (`mouse:272`, empty) match only themselves.
fn same_key(a: &str, b: &str) -> bool {
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    match (resolve_keysym(a), resolve_keysym(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn resolve_keysym(name: &str) -> Option<xkb::Keysym> {
    // `keysym_from_name` builds a C string; a NUL in a name from IPC must not reach it.
    if name.is_empty() || name.contains('\0') {
        return None;
    }
    let keysym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
    (keysym.raw() != 0).then_some(keysym)
}

/// Whether `command` can travel as `hl.dsp.exec_cmd([==[<command>]==])` and run under `sh -c`.
fn command_is_quotable(command: &str) -> bool {
    if command.trim().is_empty() || command.chars().any(char::is_control) {
        return false;
    }
    shell_quotes_balanced(command) && lua_long_string_holds(command)
}

/// Whether `[==[<text>]==]` is one Lua long string whose content is exactly `text`: the first
/// `]==]` in the text plus the closer must be the closer itself. That refuses `]==]` inside the text
/// and also a text that ends in `]==` (which would close the string one byte early).
fn lua_long_string_holds(text: &str) -> bool {
    const CLOSER: &str = "]==]";
    format!("{text}{CLOSER}").find(CLOSER) == Some(text.len())
}

/// POSIX `sh` quoting: a backslash escapes the next character outside single quotes, `'...'` is
/// literal and `"..."` honours backslashes. True when nothing is left open or dangling.
fn shell_quotes_balanced(command: &str) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut quote = Quote::None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Quote::None | Quote::Double, '\\') => {
                if chars.next().is_none() {
                    return false;
                }
            }
            (Quote::None, '\'') => quote = Quote::Single,
            (Quote::None, '"') => quote = Quote::Double,
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            _ => {}
        }
    }
    quote == Quote::None
}

/// The Lua that installs the bind. Press only: no `release`, `repeat` or `long_press`.
fn bind_lua(keys: &str, command: &str) -> String {
    format!(
        "hl.bind(\"{keys}\", hl.dsp.exec_cmd([==[{command}]==]), \
         {{ submap_universal = true, dont_inhibit = true, description = \"{DESCRIPTION}\" }})"
    )
}

/// The Lua that removes every bind spelled `keys` (`hl.unbind` ignores the submap).
fn unbind_lua(keys: &str) -> String {
    format!("hl.unbind(\"{keys}\")")
}

/// One entry of `hyprctl binds -j`, reduced to what ownership needs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ListedBind {
    modmask: u32,
    key: String,
    submap_universal: bool,
    description: String,
}

/// Parse `binds -j`. A reply that isn't a list of objects with a numeric `modmask`, a string `key`
/// and a string `description` is an error: ownership can't be decided from it, so callers fail
/// closed. The description is the only thing that tells our bind from a foreign one, and Hyprland
/// always emits it (`HyprCtl.cpp`, possibly empty), so a missing, `null` or mistyped one must not
/// read as "" (which would classify our still-active bind as foreign and let `remove` report
/// `Ok` without unbinding it).
fn parse_binds(value: &serde_json::Value) -> Result<Vec<ListedBind>, PlatformError> {
    let bad = |what: &str| PlatformError::Backend(format!("hyprland binds: {what}"));
    let list = value.as_array().ok_or_else(|| bad("not a list"))?;
    list.iter()
        .map(|entry| {
            let modmask = entry
                .get("modmask")
                .and_then(serde_json::Value::as_u64)
                .and_then(|m| u32::try_from(m).ok())
                .ok_or_else(|| bad("an entry has no modmask"))?;
            let key = entry
                .get("key")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| bad("an entry has no key"))?
                .to_owned();
            let description = entry
                .get("description")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| bad("an entry has no string description"))?
                .to_owned();
            Ok(ListedBind {
                modmask,
                key,
                // Hyprland formats the flag with `std::format("{}", bool)`: the string "true".
                submap_universal: truthy(entry.get("submap_universal")),
                description,
            })
        })
        .collect()
}

/// `"true"`, `true` and `"1"` (U9: the formatting isn't pinned by anything we control).
fn truthy(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::String(s)) => s == "true" || s == "1",
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    }
}

/// Who holds the chord, counted from one `binds -j` reply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Ownership {
    /// Binds on the chord that carry our description, in any shape.
    ours: usize,
    /// Of those, the ones shaped as we install them (`submap_universal`).
    ours_shaped: usize,
    /// Binds on the chord that are not ours.
    foreign: usize,
}

impl Ownership {
    fn classify(listed: &[ListedBind], spelling: &Spelling) -> Ownership {
        let mut owned = Ownership::default();
        for bind in listed.iter().filter(|b| spelling.is_on_chord(b)) {
            if bind.description == DESCRIPTION {
                owned.ours += 1;
                owned.ours_shaped += usize::from(bind.submap_universal);
            } else {
                owned.foreign += 1;
            }
        }
        owned
    }

    /// The chord holds exactly one bind: ours, shaped as installed.
    fn is_exactly_ours(&self) -> bool {
        self.ours == 1 && self.ours_shaped == 1 && self.foreign == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallPlan {
    /// A foreign bind holds the chord; `hl.unbind` would take it too.
    Refuse,
    /// Bind, after clearing a stale copy of ours if one is listed.
    Bind { clear_stale: bool },
}

fn install_plan(owned: &Ownership) -> InstallPlan {
    if owned.foreign > 0 {
        InstallPlan::Refuse
    } else {
        InstallPlan::Bind {
            clear_stale: owned.ours > 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemovePlan {
    /// Nothing on the chord: absence is confirmed.
    Absent,
    /// Only a foreign bind: ours is absent (B5); leave the foreign one alone.
    ForeignOnly,
    /// Only ours: unbind, then confirm.
    Unbind,
    /// Ours and a foreign bind share the chord: `hl.unbind` would remove both.
    Refuse,
}

fn remove_plan(owned: &Ownership) -> RemovePlan {
    match (owned.ours > 0, owned.foreign > 0) {
        (false, false) => RemovePlan::Absent,
        (false, true) => RemovePlan::ForeignOnly,
        (true, false) => RemovePlan::Unbind,
        (true, true) => RemovePlan::Refuse,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use serde_json::json;

    use super::*;

    const COMMAND: &str = "env CROSSPANE_RUNTIME_DIR='/run/user/1000/crosspane' \
                           '/home/u/.local/bin/crosspanectl' release";

    fn chord(mods: &[u16], key: u16) -> Chord {
        Chord {
            modifiers: mods.iter().map(|m| HidUsage::keyboard(*m)).collect(),
            key: HidUsage::keyboard(key),
        }
    }

    /// The engine's default release chord: Left Ctrl, Left Shift, Left Alt and Escape.
    fn default_chord() -> Chord {
        chord(&[0xE0, 0xE1, 0xE2], 0x29)
    }

    fn entry(
        modmask: u32,
        key: &str,
        submap_universal: serde_json::Value,
        description: &str,
    ) -> serde_json::Value {
        json!({
            "locked": false, "mouse": false, "release": false, "repeat": false,
            "modmask": modmask, "submap": "", "submap_universal": submap_universal,
            "key": key, "keycode": 0, "description": description,
            "dispatcher": "__lua", "arg": "7"
        })
    }

    fn ours_entry() -> serde_json::Value {
        entry(13, "Escape", json!("true"), DESCRIPTION)
    }

    fn foreign_entry() -> serde_json::Value {
        entry(13, "Escape", json!("false"), "owner's own bind")
    }

    // ---- parsing -----------------------------------------------------------------------------

    #[test]
    fn parses_hyprctl_binds_json() {
        // Verbatim shape of Hyprland 0.56.2's `hyprctl binds -j`.
        let reply = r#"[
        {
            "locked": false, "mouse": false, "release": false, "repeat": false,
            "longPress": false, "non_consuming": false, "auto_consuming": false,
            "has_description": true, "modmask": 12, "submap": "", "submap_universal": "false",
            "key": "Escape", "keycode": 0, "catch_all": false,
            "description": "Exit Crosspane test session", "allow_input_capture": false,
            "dispatcher": "__lua", "arg": "5"
        },
        {
            "locked": false, "mouse": false, "release": false, "repeat": false,
            "modmask": 13, "submap": "", "submap_universal": "true",
            "key": "Escape", "keycode": 0, "catch_all": false,
            "description": "crosspane-home-release", "dispatcher": "__lua", "arg": "6"
        }]"#;
        let listed = parse_binds(&serde_json::from_str(reply).unwrap()).unwrap();
        assert_eq!(
            listed,
            [
                ListedBind {
                    modmask: 12,
                    key: "Escape".into(),
                    submap_universal: false,
                    description: "Exit Crosspane test session".into(),
                },
                ListedBind {
                    modmask: 13,
                    key: "Escape".into(),
                    submap_universal: true,
                    description: DESCRIPTION.into(),
                },
            ]
        );
        assert_eq!(parse_binds(&json!([])).unwrap(), []);
    }

    #[test]
    fn submap_universal_accepts_string_bool_and_number() {
        for (value, expected) in [
            (json!("true"), true),
            (json!(true), true),
            (json!("1"), true),
            (json!(1), true),
            (json!("false"), false),
            (json!(false), false),
            (json!("0"), false),
            (json!(0), false),
            (json!("yes"), false),
            (json!(null), false),
        ] {
            let listed = parse_binds(&json!([entry(13, "Escape", value.clone(), "")])).unwrap();
            assert_eq!(
                listed.first().map(|b| b.submap_universal),
                Some(expected),
                "{value}"
            );
        }
        // A missing flag reads as false (never "shaped", so it fails closed); an empty
        // description is what Hyprland emits for a bind without one.
        let listed =
            parse_binds(&json!([{ "modmask": 4, "key": "a", "description": "" }])).unwrap();
        assert_eq!(
            listed,
            [ListedBind {
                modmask: 4,
                key: "a".into(),
                submap_universal: false,
                description: String::new(),
            }]
        );
    }

    /// An otherwise valid entry on the chord whose `description` is replaced by `description`
    /// (`None` leaves it out).
    fn entry_with_description(description: Option<serde_json::Value>) -> serde_json::Value {
        let mut e = entry(13, "Escape", json!("true"), "placeholder");
        if let Some(object) = e.as_object_mut() {
            match description {
                Some(value) => object.insert("description".into(), value),
                None => object.remove("description"),
            };
        }
        e
    }

    /// Malformed descriptions: missing, `null` and every non-string JSON type.
    fn malformed_descriptions() -> Vec<serde_json::Value> {
        vec![
            entry_with_description(None),
            entry_with_description(Some(json!(null))),
            entry_with_description(Some(json!(5))),
            entry_with_description(Some(json!(true))),
            entry_with_description(Some(json!([DESCRIPTION]))),
            entry_with_description(Some(json!({ "description": DESCRIPTION }))),
        ]
    }

    #[test]
    fn a_missing_or_mistyped_description_is_an_error() {
        // Reading it as "" would classify our still-active bind as foreign.
        assert!(parse_binds(&json!([entry_with_description(Some(json!("")))])).is_ok());
        for bad in malformed_descriptions() {
            assert!(
                matches!(parse_binds(&json!([bad])), Err(PlatformError::Backend(_))),
                "{bad}"
            );
            // One bad entry among good ones fails the whole read: nothing is classified from it.
            assert!(
                parse_binds(&json!([ours_entry(), bad, foreign_entry()])).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn unreadable_binds_replies_are_errors() {
        for bad in [
            json!({}),
            json!("ok"),
            json!(null),
            json!([42]),
            json!([{ "key": "Escape" }]),
            json!([{ "modmask": 13 }]),
            json!([{ "modmask": "13", "key": "Escape" }]),
            json!([{ "modmask": -1, "key": "Escape" }]),
            json!([{ "modmask": 13, "key": 5 }]),
        ] {
            assert!(
                matches!(parse_binds(&bad), Err(PlatformError::Backend(_))),
                "{bad}"
            );
        }
    }

    // ---- spelling ----------------------------------------------------------------------------

    #[test]
    fn default_chord_spelling_and_modmask() {
        let s = Spelling::of(&default_chord()).unwrap();
        assert_eq!(s.keys, "CTRL + SHIFT + ALT + Escape");
        assert_eq!(s.key_name, "Escape");
        assert_eq!(s.modmask, MOD_CTRL | MOD_SHIFT | MOD_ALT);
        assert_eq!(s.modmask, 13);
    }

    #[test]
    fn modifiers_fold_left_and_right_and_order_canonically() {
        // Left and right Ctrl are one CTRL; the order of the input doesn't matter.
        let s = Spelling::of(&chord(&[0xE7, 0xE4, 0xE0, 0xE5], 0x3A)).unwrap();
        assert_eq!(s.keys, "CTRL + SHIFT + SUPER + F1");
        assert_eq!(s.modmask, 4 | 1 | 64);
        let all = Spelling::of(&chord(&[0xE3, 0xE2, 0xE1, 0xE0], 0x2C)).unwrap();
        assert_eq!(all.keys, "CTRL + SHIFT + ALT + SUPER + space");
        assert_eq!(all.modmask, 1 | 4 | 8 | 64);
        // No modifiers is a plain key.
        let bare = Spelling::of(&chord(&[], 0x48)).unwrap();
        assert_eq!(bare.keys, "Pause");
        assert_eq!(bare.modmask, 0);
    }

    #[test]
    fn key_table_covers_exactly_the_design_set() {
        let named: Vec<(u16, &str)> = vec![
            (0x04, "a"),
            (0x1D, "z"),
            (0x1E, "1"),
            (0x26, "9"),
            (0x27, "0"),
            (0x28, "Return"),
            (0x29, "Escape"),
            (0x2A, "BackSpace"),
            (0x2B, "Tab"),
            (0x2C, "space"),
            (0x3A, "F1"),
            (0x45, "F12"),
            (0x46, "Print"),
            (0x48, "Pause"),
            (0x49, "Insert"),
            (0x4A, "Home"),
            (0x4B, "Page_Up"),
            (0x4C, "Delete"),
            (0x4D, "End"),
            (0x4E, "Page_Down"),
        ];
        for (id, name) in named {
            assert_eq!(key_name(HidUsage::keyboard(id)), Some(name), "{id:#x}");
        }
        let mut mapped = 0;
        for id in 0..=0xFFu16 {
            if key_name(HidUsage::keyboard(id)).is_some() {
                mapped += 1;
            }
        }
        // 26 letters, 10 digits, 12 function keys, and 13 named keys (Return, Escape, BackSpace,
        // Tab, space, Print, Pause, Insert, Home, Page_Up, Delete, End, Page_Down).
        assert_eq!(mapped, 26 + 10 + 12 + 13);
        // Everything else is refused: CapsLock, Scroll Lock, arrows, keypad, the modifiers.
        for id in [
            0x00, 0x01, 0x2D, 0x39, 0x47, 0x4F, 0x50, 0x53, 0x65, 0xE0, 0xE7,
        ] {
            assert_eq!(key_name(HidUsage::keyboard(id)), None, "{id:#x}");
        }
        // Another usage page never maps.
        assert_eq!(
            key_name(HidUsage {
                page: 0x0C,
                id: 0x29
            }),
            None
        );
    }

    #[test]
    fn unmapped_chords_are_unsupported() {
        for refused in [
            // An arrow key.
            chord(&[0xE0], 0x4F),
            // CapsLock.
            chord(&[0xE0], 0x39),
            // A modifier as the key.
            chord(&[0xE0], 0xE1),
            // A non-modifier as a modifier.
            chord(&[0x04], 0x29),
            // The wrong usage page.
            Chord {
                modifiers: vec![HidUsage {
                    page: 0x0C,
                    id: 0xE0,
                }],
                key: HidUsage::keyboard(0x29),
            },
            Chord {
                modifiers: vec![HidUsage::keyboard(0xE0)],
                key: HidUsage {
                    page: 0x0C,
                    id: 0x29,
                },
            },
        ] {
            assert!(
                matches!(Spelling::of(&refused), Err(PlatformError::Unsupported(_))),
                "{refused:?}"
            );
        }
    }

    // ---- key identity ------------------------------------------------------------------------

    #[test]
    fn same_key_folds_case_and_aliases_like_hyprland() {
        assert!(same_key("Escape", "Escape"));
        assert!(same_key("escape", "Escape"));
        assert!(same_key("ESCAPE", "Escape"));
        assert!(same_key("A", "a"));
        assert!(same_key("page_up", "Page_Up"));
        // `Prior` is the canonical xkb name of Page_Up's keysym.
        assert!(same_key("Prior", "Page_Up"));
        assert!(same_key("Next", "Page_Down"));
        assert!(!same_key("Escape", "F1"));
        assert!(!same_key("a", "b"));
        // Not keysyms: they match only themselves.
        assert!(same_key("mouse:272", "mouse:272"));
        assert!(!same_key("mouse:272", "Escape"));
        assert!(!same_key("", "Escape"));
        assert!(!same_key("code:9", "Escape"));
        // A NUL in a name from IPC is harmless.
        assert!(!same_key("Esc\0ape", "Escape"));
    }

    // ---- quoting -----------------------------------------------------------------------------

    #[test]
    fn quoting_accepts_balanced_shell_commands() {
        for ok in [
            COMMAND,
            "true",
            "touch /tmp/marker",
            "echo 'it is' fine",
            "echo \"a b\"",
            "echo \"it's\"",
            "echo it\\'s",
            "echo 'a \"quoted\" b'",
            "'/path with space/crosspanectl' release",
            "a ] b ]= c",
            // Partial closers are fine as long as the text can't close the Lua string early.
            "echo a]",
            "echo a]=",
            "echo a]==x",
        ] {
            assert!(command_is_quotable(ok), "{ok}");
        }
    }

    #[test]
    fn quoting_refuses_what_cannot_travel() {
        for refused in [
            // A `'` that is not closed: `sh -c` would fail at press time.
            "echo it's",
            "echo '/run/user/it's/dir' release",
            "echo \"unterminated",
            "echo trailing\\",
            "echo \"dangling\\",
            // Lua long-string breakout.
            "true ]==] os.exit()",
            "]==]",
            // Ending in `]==` closes the string one byte early once the closer is appended.
            "true ]==",
            "]==",
            // Control characters.
            "true\nfalse",
            "true\r",
            "tr\tue",
            "true\0",
            "\u{7f}",
            // Nothing to run.
            "",
            "   ",
        ] {
            assert!(!command_is_quotable(refused), "{refused:?}");
        }
    }

    #[test]
    fn new_refuses_unquotable_commands_and_unmapped_chords() {
        let ipc = FakeCompositor::default();
        assert!(Core::new(ipc.clone(), &default_chord(), COMMAND).is_ok());
        assert!(matches!(
            Core::new(ipc.clone(), &default_chord(), "echo it's"),
            Err(PlatformError::Unsupported(_))
        ));
        assert!(matches!(
            Core::new(ipc.clone(), &default_chord(), "x ]==] y"),
            Err(PlatformError::Unsupported(_))
        ));
        assert!(matches!(
            Core::new(ipc, &chord(&[0xE0], 0x4F), COMMAND),
            Err(PlatformError::Unsupported(_))
        ));
    }

    // ---- generated Lua -----------------------------------------------------------------------

    #[test]
    fn bind_lua_is_universal_uninhibited_and_press_only() {
        let lua = bind_lua("CTRL + SHIFT + ALT + Escape", COMMAND);
        assert_eq!(
            lua,
            "hl.bind(\"CTRL + SHIFT + ALT + Escape\", hl.dsp.exec_cmd([==[env \
             CROSSPANE_RUNTIME_DIR='/run/user/1000/crosspane' \
             '/home/u/.local/bin/crosspanectl' release]==]), \
             { submap_universal = true, dont_inhibit = true, \
             description = \"crosspane-home-release\" })"
        );
        // Fires whatever submap is active, and under a shortcuts inhibitor.
        assert!(lua.contains("submap_universal = true"));
        assert!(lua.contains("dont_inhibit = true"));
        // Press only: nothing that makes a second, releasing or repeating bind.
        let options = lua.rsplit_once('{').map_or("", |(_, o)| o);
        for forbidden in ["release", "repeat", "long_press", "locked", "mouse"] {
            assert!(!options.contains(&format!("{forbidden} =")), "{forbidden}");
        }
        assert_eq!(
            options.matches(" = ").count(),
            3,
            "exactly submap_universal, dont_inhibit and description: {options}"
        );
        assert_eq!(lua.matches("hl.bind(").count(), 1);
        assert!(!lua.contains('\n'));
    }

    #[test]
    fn unbind_lua_names_only_the_chord() {
        assert_eq!(
            unbind_lua("CTRL + SHIFT + ALT + Escape"),
            "hl.unbind(\"CTRL + SHIFT + ALT + Escape\")"
        );
    }

    // ---- ownership decisions -----------------------------------------------------------------

    fn ownership(listed: &[serde_json::Value]) -> Ownership {
        let s = Spelling::of(&default_chord()).unwrap();
        let listed = parse_binds(&serde_json::Value::Array(listed.to_vec())).unwrap();
        Ownership::classify(&listed, &s)
    }

    #[test]
    fn ownership_none() {
        let o = ownership(&[]);
        assert_eq!(o, Ownership::default());
        assert_eq!(install_plan(&o), InstallPlan::Bind { clear_stale: false });
        assert_eq!(remove_plan(&o), RemovePlan::Absent);
        assert!(!o.is_exactly_ours());
        // Binds elsewhere (the nest's CTRL + ALT + Escape; our description at another chord;
        // another key on the same modifiers) are not on the chord.
        let o = ownership(&[
            entry(12, "Escape", json!("false"), "Exit Crosspane test session"),
            entry(5, "Escape", json!("true"), DESCRIPTION),
            entry(13, "F1", json!("false"), "elsewhere"),
            entry(13, "mouse:272", json!("false"), ""),
        ]);
        assert_eq!(o, Ownership::default());
        assert_eq!(remove_plan(&o), RemovePlan::Absent);
    }

    #[test]
    fn ownership_ours_only() {
        let o = ownership(&[ours_entry()]);
        assert_eq!(
            o,
            Ownership {
                ours: 1,
                ours_shaped: 1,
                foreign: 0
            }
        );
        assert!(o.is_exactly_ours());
        assert_eq!(install_plan(&o), InstallPlan::Bind { clear_stale: true });
        assert_eq!(remove_plan(&o), RemovePlan::Unbind);
        // Ours in the wrong shape (not universal) is ours, but not "exactly ours".
        let o = ownership(&[entry(13, "Escape", json!("false"), DESCRIPTION)]);
        assert_eq!(o.ours, 1);
        assert!(!o.is_exactly_ours());
        assert_eq!(remove_plan(&o), RemovePlan::Unbind);
        // Two copies of ours: unbind removes both; neither is "exactly ours".
        let o = ownership(&[ours_entry(), ours_entry()]);
        assert!(!o.is_exactly_ours());
        assert_eq!(remove_plan(&o), RemovePlan::Unbind);
    }

    #[test]
    fn ownership_foreign_only() {
        for foreign in [
            foreign_entry(),
            // Another spelling of the same key on the same chord.
            entry(13, "escape", json!("false"), ""),
            // In some other submap: `hl.unbind` ignores the submap, so it still collides.
            json!({ "modmask": 13, "key": "Escape", "submap": "resize", "description": "" }),
        ] {
            let o = ownership(std::slice::from_ref(&foreign));
            assert_eq!(
                o,
                Ownership {
                    ours: 0,
                    ours_shaped: 0,
                    foreign: 1
                },
                "{foreign}"
            );
            assert_eq!(install_plan(&o), InstallPlan::Refuse);
            // B5: ours is absent, so removal succeeds and leaves the foreign bind alone.
            assert_eq!(remove_plan(&o), RemovePlan::ForeignOnly);
            assert!(!o.is_exactly_ours());
        }
    }

    #[test]
    fn ownership_both() {
        for listed in [
            vec![ours_entry(), foreign_entry()],
            vec![foreign_entry(), ours_entry()],
        ] {
            let o = ownership(&listed);
            assert_eq!(
                o,
                Ownership {
                    ours: 1,
                    ours_shaped: 1,
                    foreign: 1
                }
            );
            assert_eq!(install_plan(&o), InstallPlan::Refuse);
            assert_eq!(remove_plan(&o), RemovePlan::Refuse);
            // Ours is listed but the chord is shared: not "exactly ours".
            assert!(!o.is_exactly_ours());
        }
    }

    // ---- flows against a fake compositor -----------------------------------------------------

    /// One scripted failure for the next matching call.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Fail {
        Read,
        Eval,
    }

    #[derive(Debug, Default)]
    struct Fake {
        binds: Vec<serde_json::Value>,
        /// Every Lua chunk run, in order.
        lua: Vec<String>,
        reads: usize,
        /// Fail the n-th (0-based) call of that kind.
        fail: Vec<(Fail, usize)>,
        evals: usize,
        /// Make `hl.bind` register a bind with a wrong shape (not universal).
        bind_wrong_shape: bool,
        /// Make `hl.unbind` do nothing.
        unbind_is_noop: bool,
        /// Add a foreign bind on the chord right after the next `hl.bind` (a race).
        foreign_after_bind: bool,
        /// A raw (possibly malformed) entry that appears in the table right after `hl.unbind`
        /// and right after `hl.bind`, so the verifying read sees it.
        append_after_unbind: Option<serde_json::Value>,
        append_after_bind: Option<serde_json::Value>,
    }

    #[derive(Clone, Debug, Default)]
    struct FakeCompositor(Rc<RefCell<Fake>>);

    impl FakeCompositor {
        fn with(binds: Vec<serde_json::Value>) -> FakeCompositor {
            let fake = FakeCompositor::default();
            fake.0.borrow_mut().binds = binds;
            fake
        }

        fn lua(&self) -> Vec<String> {
            self.0.borrow().lua.clone()
        }

        fn binds(&self) -> Vec<serde_json::Value> {
            self.0.borrow().binds.clone()
        }

        fn descriptions(&self) -> Vec<String> {
            self.binds()
                .iter()
                .filter_map(|b| b.get("description")?.as_str().map(str::to_owned))
                .collect()
        }
    }

    impl Compositor for FakeCompositor {
        fn binds(&self) -> Result<serde_json::Value, PlatformError> {
            let mut fake = self.0.borrow_mut();
            let n = fake.reads;
            fake.reads += 1;
            if fake.fail.contains(&(Fail::Read, n)) {
                return Err(PlatformError::Timeout);
            }
            Ok(serde_json::Value::Array(fake.binds.clone()))
        }

        fn run_lua(&self, lua: &str) -> Result<(), PlatformError> {
            let mut fake = self.0.borrow_mut();
            let n = fake.evals;
            fake.evals += 1;
            fake.lua.push(lua.to_owned());
            if fake.fail.contains(&(Fail::Eval, n)) {
                return Err(PlatformError::Backend("hyprland eval: boom".into()));
            }
            if lua.starts_with("hl.unbind(") {
                if !fake.unbind_is_noop {
                    // Every bind on the chord goes, whoever owns it (modmask 13, Escape).
                    fake.binds.retain(|b| {
                        !(b.get("modmask").and_then(serde_json::Value::as_u64) == Some(13)
                            && b.get("key").and_then(serde_json::Value::as_str) == Some("Escape"))
                    });
                }
                if let Some(raw) = fake.append_after_unbind.clone() {
                    fake.binds.push(raw);
                }
            } else if lua.starts_with("hl.bind(") {
                let universal = if fake.bind_wrong_shape {
                    "false"
                } else {
                    "true"
                };
                fake.binds
                    .push(entry(13, "Escape", json!(universal), DESCRIPTION));
                if fake.foreign_after_bind {
                    fake.binds.push(foreign_entry());
                }
                if let Some(raw) = fake.append_after_bind.clone() {
                    fake.binds.push(raw);
                }
            }
            Ok(())
        }
    }

    fn core(fake: &FakeCompositor) -> Core<FakeCompositor> {
        Core::new(fake.clone(), &default_chord(), COMMAND).unwrap()
    }

    #[test]
    fn install_on_a_clean_chord_binds_and_verifies() {
        let fake = FakeCompositor::with(vec![entry(12, "Escape", json!("false"), "owner")]);
        let c = core(&fake);
        assert!(!c.installed().unwrap());
        c.install().unwrap();
        assert!(c.installed().unwrap());
        // No stale copy: no unbind, one bind.
        let lua = fake.lua();
        assert_eq!(lua.len(), 1, "{lua:?}");
        assert!(lua[0].starts_with("hl.bind(\"CTRL + SHIFT + ALT + Escape\""));
        // The owner's bind on CTRL + ALT is untouched.
        assert_eq!(fake.descriptions(), ["owner", DESCRIPTION]);
    }

    #[test]
    fn install_clears_a_stale_copy_of_ours_first() {
        let fake = FakeCompositor::with(vec![ours_entry(), ours_entry()]);
        let c = core(&fake);
        c.install().unwrap();
        let lua = fake.lua();
        assert_eq!(lua.len(), 2, "{lua:?}");
        assert!(lua[0].starts_with("hl.unbind("));
        assert!(lua[1].starts_with("hl.bind("));
        assert_eq!(fake.descriptions(), [DESCRIPTION]);
    }

    #[test]
    fn install_refuses_a_collision_and_touches_nothing() {
        for existing in [
            vec![foreign_entry()],
            vec![foreign_entry(), ours_entry()],
            vec![ours_entry(), foreign_entry()],
        ] {
            let fake = FakeCompositor::with(existing.clone());
            let err = core(&fake).install().unwrap_err();
            assert!(matches!(err, PlatformError::Backend(_)), "{err}");
            assert!(fake.lua().is_empty(), "{:?}", fake.lua());
            assert_eq!(fake.binds(), existing);
        }
    }

    #[test]
    fn install_fails_closed_on_a_read_failure() {
        // The collision check can't be made: nothing is bound.
        let fake = FakeCompositor::default();
        fake.0.borrow_mut().fail = vec![(Fail::Read, 0)];
        assert!(matches!(core(&fake).install(), Err(PlatformError::Timeout)));
        assert!(fake.lua().is_empty());
        assert!(fake.binds().is_empty());
    }

    #[test]
    fn install_reports_a_failed_eval_and_a_failed_verification() {
        // The bind itself fails.
        let fake = FakeCompositor::default();
        fake.0.borrow_mut().fail = vec![(Fail::Eval, 0)];
        assert!(matches!(
            core(&fake).install(),
            Err(PlatformError::Backend(_))
        ));
        assert!(fake.binds().is_empty());

        // The verifying read fails: not "Ok" even though the bind landed. No rollback here; the
        // engine always follows a failed install with a removal.
        let fake = FakeCompositor::default();
        fake.0.borrow_mut().fail = vec![(Fail::Read, 1)];
        assert!(core(&fake).install().is_err());
        assert_eq!(fake.descriptions(), [DESCRIPTION]);

        // Listed in the wrong shape.
        let fake = FakeCompositor::default();
        fake.0.borrow_mut().bind_wrong_shape = true;
        assert!(core(&fake).install().is_err());

        // A foreign bind raced in on the chord.
        let fake = FakeCompositor::default();
        fake.0.borrow_mut().foreign_after_bind = true;
        assert!(core(&fake).install().is_err());

        // A stale copy that can't be cleared is never papered over by a second bind.
        let fake = FakeCompositor::with(vec![ours_entry()]);
        fake.0.borrow_mut().fail = vec![(Fail::Eval, 0)];
        assert!(core(&fake).install().is_err());
        assert_eq!(fake.lua().len(), 1);
    }

    #[test]
    fn remove_ours_only_unbinds_and_verifies() {
        let fake = FakeCompositor::with(vec![
            entry(12, "Escape", json!("false"), "owner on another chord"),
            ours_entry(),
        ]);
        let c = core(&fake);
        c.remove().unwrap();
        assert_eq!(fake.descriptions(), ["owner on another chord"]);
        assert_eq!(fake.lua().len(), 1);
        assert!(!c.installed().unwrap());
    }

    #[test]
    fn remove_is_idempotent() {
        let fake = FakeCompositor::with(vec![ours_entry()]);
        let c = core(&fake);
        c.remove().unwrap();
        c.remove().unwrap();
        c.remove().unwrap();
        assert!(fake.binds().is_empty());
        // Only the first call changed anything.
        assert_eq!(fake.lua().len(), 1);
    }

    #[test]
    fn remove_with_nothing_listed_is_ok_without_an_unbind() {
        let fake = FakeCompositor::default();
        core(&fake).remove().unwrap();
        assert!(fake.lua().is_empty());
    }

    #[test]
    fn remove_with_only_a_foreign_bind_is_ok_and_leaves_it() {
        // B5: ours is absent, the foreign bind stays, the fence may clear.
        let fake = FakeCompositor::with(vec![foreign_entry()]);
        core(&fake).remove().unwrap();
        assert!(fake.lua().is_empty(), "{:?}", fake.lua());
        assert_eq!(fake.descriptions(), ["owner's own bind"]);
    }

    #[test]
    fn remove_refuses_when_ours_and_a_foreign_bind_share_the_chord() {
        let fake = FakeCompositor::with(vec![ours_entry(), foreign_entry()]);
        let err = core(&fake).remove().unwrap_err();
        assert!(matches!(err, PlatformError::Backend(_)), "{err}");
        // Nothing was unbound: the owner's bind is intact and ours is still listed.
        assert!(fake.lua().is_empty());
        assert_eq!(fake.descriptions(), [DESCRIPTION, "owner's own bind"]);
        // A config reload drops runtime binds; the next retry finds the chord clean.
        fake.0.borrow_mut().binds.clear();
        core(&fake).remove().unwrap();
    }

    #[test]
    fn remove_fails_closed_when_ownership_is_unknown() {
        // The binds read fails: nothing is unbound, and the caller keeps its fence.
        let fake = FakeCompositor::with(vec![ours_entry()]);
        fake.0.borrow_mut().fail = vec![(Fail::Read, 0)];
        assert!(matches!(core(&fake).remove(), Err(PlatformError::Timeout)));
        assert!(fake.lua().is_empty());
        assert_eq!(fake.binds().len(), 1);

        // The reply is unreadable.
        struct Garbled;
        impl Compositor for Garbled {
            fn binds(&self) -> Result<serde_json::Value, PlatformError> {
                Ok(json!({ "error": "x" }))
            }
            fn run_lua(&self, _: &str) -> Result<(), PlatformError> {
                Err(PlatformError::Backend("must not be called".into()))
            }
        }
        let garbled = Core::new(Garbled, &default_chord(), COMMAND).unwrap();
        assert!(matches!(garbled.remove(), Err(PlatformError::Backend(_))));
        assert!(garbled.installed().is_err());
        assert!(garbled.install().is_err());
    }

    #[test]
    fn a_malformed_description_in_the_initial_read_fails_closed() {
        // Our still-active bind listed without a usable description must not read as a foreign
        // bind: `remove` would then report `Ok` (absence "confirmed") and clear the teardown fence
        // with the bind still bound.
        for bad in malformed_descriptions() {
            for listed in [
                vec![bad.clone()],
                vec![ours_entry(), bad.clone()],
                vec![bad.clone(), foreign_entry()],
            ] {
                let fake = FakeCompositor::with(listed.clone());
                assert!(core(&fake).remove().is_err(), "{listed:?}");
                assert!(core(&fake).install().is_err(), "{listed:?}");
                assert!(core(&fake).installed().is_err(), "{listed:?}");
                // Nothing was changed on the strength of an unreadable table.
                assert!(fake.lua().is_empty(), "{:?}", fake.lua());
                assert_eq!(fake.binds(), listed);
            }
        }
    }

    #[test]
    fn a_malformed_description_in_the_verifying_read_fails_closed() {
        for bad in malformed_descriptions() {
            // `remove`: the table reads fine, the unbind runs, the read that must confirm absence
            // is malformed: not `Ok`.
            let fake = FakeCompositor::with(vec![ours_entry()]);
            fake.0.borrow_mut().append_after_unbind = Some(bad.clone());
            assert!(core(&fake).remove().is_err(), "{bad}");
            assert_eq!(fake.lua().len(), 1);

            // `install`: the same, in the read that verifies the bind.
            let fake = FakeCompositor::default();
            fake.0.borrow_mut().append_after_bind = Some(bad.clone());
            assert!(core(&fake).install().is_err(), "{bad}");
            assert_eq!(fake.lua().len(), 1);

            // `install` over a stale copy: the unbind and the bind run, the verifying read is
            // malformed.
            let fake = FakeCompositor::with(vec![ours_entry()]);
            fake.0.borrow_mut().append_after_bind = Some(bad.clone());
            assert!(core(&fake).install().is_err(), "{bad}");
            assert_eq!(fake.lua().len(), 2);
        }
    }

    #[test]
    fn remove_does_not_report_ok_when_absence_is_not_verified() {
        // The unbind ran but the bind is still listed.
        let fake = FakeCompositor::with(vec![ours_entry()]);
        fake.0.borrow_mut().unbind_is_noop = true;
        assert!(core(&fake).remove().is_err());

        // The unbind ran, but the verifying read failed.
        let fake = FakeCompositor::with(vec![ours_entry()]);
        fake.0.borrow_mut().fail = vec![(Fail::Read, 1)];
        assert!(core(&fake).remove().is_err());

        // The unbind itself failed.
        let fake = FakeCompositor::with(vec![ours_entry()]);
        fake.0.borrow_mut().fail = vec![(Fail::Eval, 0)];
        assert!(core(&fake).remove().is_err());
        assert_eq!(fake.binds().len(), 1);
    }

    #[test]
    fn installed_is_true_only_for_exactly_our_bind() {
        let cases: [(Vec<serde_json::Value>, bool); 7] = [
            (vec![], false),
            (vec![ours_entry()], true),
            (vec![ours_entry(), ours_entry()], false),
            (vec![foreign_entry()], false),
            (vec![ours_entry(), foreign_entry()], false),
            (
                vec![entry(13, "Escape", json!("false"), DESCRIPTION)],
                false,
            ),
            (vec![entry(5, "Escape", json!("true"), DESCRIPTION)], false),
        ];
        for (listed, expected) in cases {
            let fake = FakeCompositor::with(listed.clone());
            assert_eq!(core(&fake).installed().unwrap(), expected, "{listed:?}");
            // `installed` only reads.
            assert!(fake.lua().is_empty());
        }
        let fake = FakeCompositor::default();
        fake.0.borrow_mut().fail = vec![(Fail::Read, 0)];
        assert!(core(&fake).installed().is_err());
    }

    #[test]
    fn keys_reports_the_hyprland_spelling() {
        let fake = FakeCompositor::default();
        assert_eq!(core(&fake).spelling.keys, "CTRL + SHIFT + ALT + Escape");
    }

    #[test]
    fn home_bind_can_be_shared_between_the_agent_s_threads() {
        // The agent re-verifies from its housekeeping pass and its IPC event thread.
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<HomeBind>();
    }

    #[test]
    fn home_bind_is_a_thin_wrapper_over_the_ipc() {
        // The public type works over a `HyprIpc` whose socket doesn't exist: construction never
        // connects, and every method reports the failure instead of panicking or hanging.
        let ipc = HyprIpc::new(
            "no-such-instance",
            std::path::Path::new("/nonexistent-runtime-dir"),
            std::time::Duration::from_millis(100),
        );
        let bind = HomeBind::new(ipc, &default_chord(), COMMAND).unwrap();
        assert_eq!(bind.keys(), "CTRL + SHIFT + ALT + Escape");
        assert!(bind.install().is_err());
        assert!(bind.remove().is_err());
        assert!(bind.installed().is_err());
    }
}
