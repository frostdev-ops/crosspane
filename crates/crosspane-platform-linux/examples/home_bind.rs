//! Install, remove or inspect the home bind (WP-2.43 §2.9) in the Hyprland instance named by
//! `$HYPRLAND_INSTANCE_SIGNATURE`, for nested-compositor scripts. Refuses to run (exit 2) without
//! `CROSSPANE_NESTED_HYPR=1`, which `eval "$(scripts/hypr-nested.sh env)"` sets, and unless the
//! environment really is a running nest started by that script: no inherited `WAYLAND_SOCKET`, and
//! the signature and `$WAYLAND_DISPLAY` belong to one instance recorded by the script. So it can
//! never put a keybind into the owner's live session by accident.
//!
//! ```text
//! home_bind install   [--chord CTRL+SHIFT+ALT+Escape] [--command 'sh command']
//! home_bind remove    [--chord CHORD]
//! home_bind installed [--chord CHORD]    prints `true` or `false`
//! home_bind keys      [--chord CHORD]    prints the Hyprland key string
//! ```
//!
//! Exit status: 0 success, 1 the operation failed (the reason is on stderr), 2 usage. The default
//! command is `true`: it makes the bind observable (`hyprctl binds -j`) without doing anything.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("home_bind drives Hyprland: Linux only");
}

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(imp::run());
}

#[cfg(target_os = "linux")]
mod imp {
    use crosspane_platform::Chord;
    use crosspane_platform_linux::hyprland::home_bind::HomeBind;
    use crosspane_platform_linux::hyprland::ipc::HyprIpc;
    use crosspane_types::hid::HidUsage;

    const DEFAULT_CHORD: &str = "CTRL+SHIFT+ALT+Escape";
    const DEFAULT_COMMAND: &str = "true";
    const USAGE: &str = "usage: home_bind install|remove|installed|keys \
                         [--chord CTRL+SHIFT+ALT+Escape] [--command 'sh command']";

    /// `Ok` only if `$HYPRLAND_INSTANCE_SIGNATURE` and `$WAYLAND_DISPLAY` belong to one running nest
    /// started by `scripts/hypr-nested.sh`, and no `WAYLAND_SOCKET` is inherited:
    /// - Hyprland's own `hyprland.lock` for the signature (compositor pid, Wayland socket name)
    ///   must name `$WAYLAND_DISPLAY`;
    /// - `$XDG_RUNTIME_DIR/crosspane-hypr-<name>/` (`env` and `pid`, written by the script) must
    ///   record the same signature, socket and pid, and that pid must be running. The live session
    ///   has no such directory, so its signature is refused.
    ///
    /// (`tests/home_bind.rs` carries the same check, with its own tests.)
    fn verify_nest() -> Result<(), String> {
        if let Some(fd) = std::env::var_os("WAYLAND_SOCKET") {
            return Err(format!(
                "WAYLAND_SOCKET={fd:?} is set; use `eval \"$(scripts/hypr-nested.sh env)\"`, which unsets it"
            ));
        }
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let runtime = std::path::PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?,
        );
        let display = var("WAYLAND_DISPLAY").ok_or("WAYLAND_DISPLAY is not set")?;
        let signature = var("HYPRLAND_INSTANCE_SIGNATURE")
            .filter(|s| s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
            .ok_or("HYPRLAND_INSTANCE_SIGNATURE is not set or not a plain signature")?;
        let lock_path = runtime.join("hypr").join(&signature).join("hyprland.lock");
        let lock = std::fs::read_to_string(&lock_path)
            .map_err(|e| format!("can't read {}: {e}", lock_path.display()))?;
        let mut lines = lock.lines();
        let pid = lines.next().unwrap_or_default().trim();
        let lock_display = lines.next().unwrap_or_default().trim();
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) || lock_display != display {
            return Err(format!(
                "instance {signature} serves {lock_display:?}, but WAYLAND_DISPLAY is {display:?}"
            ));
        }
        let states = std::fs::read_dir(&runtime)
            .map_err(|e| format!("can't list {}: {e}", runtime.display()))?;
        for state in states.flatten() {
            if !state
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("crosspane-hypr-"))
            {
                continue;
            }
            let env = std::fs::read_to_string(state.path().join("env")).unwrap_or_default();
            let state_pid = std::fs::read_to_string(state.path().join("pid")).unwrap_or_default();
            let exported = |name: &str| {
                env.lines().find_map(|line| {
                    line.strip_prefix("export ")?
                        .strip_prefix(name)?
                        .strip_prefix('=')
                })
            };
            if exported("HYPRLAND_INSTANCE_SIGNATURE") == Some(signature.as_str())
                && exported("WAYLAND_DISPLAY") == Some(display.as_str())
                && state_pid.trim() == pid
                && std::path::Path::new("/proc").join(pid).exists()
            {
                return Ok(());
            }
        }
        Err(format!(
            "no running nest started by scripts/hypr-nested.sh owns instance {signature} on \
             {display} (is this the live session?)"
        ))
    }

    pub fn run() -> i32 {
        if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
            eprintln!("home_bind: nested compositors only (CROSSPANE_NESTED_HYPR=1)");
            return 2;
        }
        if let Err(why) = verify_nest() {
            eprintln!("home_bind: refusing to run, not a nest from scripts/hypr-nested.sh: {why}");
            return 2;
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        let Some((action, options)) = args.split_first() else {
            eprintln!("{USAGE}");
            return 2;
        };
        let mut chord = DEFAULT_CHORD.to_owned();
        let mut command = DEFAULT_COMMAND.to_owned();
        let mut options = options.iter();
        while let Some(flag) = options.next() {
            let slot = match flag.as_str() {
                "--chord" => &mut chord,
                "--command" => &mut command,
                _ => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            };
            let Some(value) = options.next() else {
                eprintln!("{USAGE}");
                return 2;
            };
            value.clone_into(slot);
        }
        let Some(chord) = parse_chord(&chord) else {
            eprintln!("home_bind: can't read the chord; {USAGE}");
            return 2;
        };
        let ipc = match HyprIpc::from_env() {
            Ok(ipc) => ipc,
            Err(e) => {
                eprintln!("home_bind: {e}");
                return 1;
            }
        };
        let bind = match HomeBind::new(ipc, &chord, &command) {
            Ok(bind) => bind,
            Err(e) => {
                eprintln!("home_bind: {e}");
                return 1;
            }
        };
        let outcome = match action.as_str() {
            "install" => bind
                .install()
                .map(|()| format!("installed {}", bind.keys())),
            "remove" => bind.remove().map(|()| format!("removed {}", bind.keys())),
            "installed" => bind.installed().map(|yes| yes.to_string()),
            "keys" => Ok(bind.keys().to_owned()),
            _ => {
                eprintln!("{USAGE}");
                return 2;
            }
        };
        match outcome {
            Ok(line) => {
                println!("{line}");
                0
            }
            Err(e) => {
                eprintln!("home_bind: {e}");
                1
            }
        }
    }

    /// `CTRL+SHIFT+ALT+Escape`: modifier names, then a key name, joined by `+`.
    fn parse_chord(text: &str) -> Option<Chord> {
        let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let key = key_usage(parts.pop()?)?;
        let modifiers = parts
            .into_iter()
            .map(|name| match name.to_ascii_uppercase().as_str() {
                "CTRL" => Some(HidUsage::keyboard(0xE0)),
                "SHIFT" => Some(HidUsage::keyboard(0xE1)),
                "ALT" => Some(HidUsage::keyboard(0xE2)),
                "SUPER" => Some(HidUsage::keyboard(0xE3)),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Chord { modifiers, key })
    }

    fn key_usage(name: &str) -> Option<HidUsage> {
        let lower = name.to_ascii_lowercase();
        let id = match lower.as_str() {
            "return" => 0x28,
            "escape" => 0x29,
            "backspace" => 0x2A,
            "tab" => 0x2B,
            "space" => 0x2C,
            "print" => 0x46,
            "pause" => 0x48,
            "insert" => 0x49,
            "home" => 0x4A,
            "page_up" => 0x4B,
            "delete" => 0x4C,
            "end" => 0x4D,
            "page_down" => 0x4E,
            other => {
                let mut chars = other.chars();
                match (chars.next(), chars.next()) {
                    (Some(c @ 'a'..='z'), None) => 0x04 + u16::from(c as u8 - b'a'),
                    (Some('0'), None) => 0x27,
                    (Some(c @ '1'..='9'), None) => 0x1E + u16::from(c as u8 - b'1'),
                    (Some('f'), _) => match other.get(1..)?.parse::<u16>().ok()? {
                        n @ 1..=12 => 0x3A + (n - 1),
                        _ => return None,
                    },
                    _ => return None,
                }
            }
        };
        Some(HidUsage::keyboard(id))
    }
}
