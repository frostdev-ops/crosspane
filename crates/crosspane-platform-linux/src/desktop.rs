//! Which Linux desktop this agent runs in (WP-G1.1). The agent picks its backend from this and
//! never falls back to another compositor's backend: an unsupported desktop is an error.

use crosspane_platform::PlatformError;

/// A Linux desktop Crosspane has a backend for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinuxDesktop {
    /// Hyprland: its own IPC and protocols (the original Linux backend).
    Hyprland,
    /// GNOME Shell (Mutter) on Wayland: portals, EIS, PipeWire and the Crosspane Shell extension.
    Gnome,
    /// KDE Plasma (KWin) on Wayland: portals, EIS and PipeWire (KDE-v0).
    Kde,
}

/// The process environment the detection reads, captured once so detection is a pure function.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopEnv {
    /// `XDG_CURRENT_DESKTOP`: a colon-separated list, e.g. `GNOME`, `ubuntu:GNOME`, `KDE`,
    /// `Hyprland`.
    pub xdg_current_desktop: Option<String>,
    /// `XDG_SESSION_TYPE`, e.g. `wayland` or `x11`.
    pub xdg_session_type: Option<String>,
    /// Whether `HYPRLAND_INSTANCE_SIGNATURE` is set and non-empty.
    pub hyprland_signature: bool,
    /// Whether `WAYLAND_DISPLAY` is set and non-empty.
    pub wayland_display: bool,
}

impl DesktopEnv {
    /// Read the variables from this process's environment.
    pub fn from_process() -> DesktopEnv {
        DesktopEnv {
            xdg_current_desktop: non_empty_string("XDG_CURRENT_DESKTOP"),
            xdg_session_type: non_empty_string("XDG_SESSION_TYPE"),
            hyprland_signature: is_set("HYPRLAND_INSTANCE_SIGNATURE"),
            wayland_display: is_set("WAYLAND_DISPLAY"),
        }
    }
}

/// Decide the desktop.
///
/// - A Hyprland signature wins (Hyprland sets it; nothing else does).
/// - Otherwise the `XDG_CURRENT_DESKTOP` entries decide, compared case-insensitively: any entry
///   `GNOME` gives [`LinuxDesktop::Gnome`]; any entry `KDE` gives [`LinuxDesktop::Kde`]; any entry
///   `Hyprland` without a signature is an error (the IPC can't be reached).
/// - GNOME and KDE also require a Wayland session: `XDG_SESSION_TYPE=wayland` or, when that is
///   unset, `WAYLAND_DISPLAY`. X11 sessions are [`PlatformError::Unsupported`].
/// - Anything else is [`PlatformError::Unsupported`]. Never guess.
pub fn detect(env: &DesktopEnv) -> Result<LinuxDesktop, PlatformError> {
    if env.hyprland_signature {
        return Ok(LinuxDesktop::Hyprland);
    }
    let entries: Vec<&str> = env
        .xdg_current_desktop
        .as_deref()
        .unwrap_or_default()
        .split(':')
        .map(str::trim)
        .collect();
    let has = |name: &str| entries.iter().any(|entry| entry.eq_ignore_ascii_case(name));
    if has("Hyprland") {
        return Err(PlatformError::Unsupported(NO_HYPRLAND_SIGNATURE));
    }
    let desktop = match (has("GNOME"), has("KDE")) {
        (true, false) => LinuxDesktop::Gnome,
        (false, true) => LinuxDesktop::Kde,
        // Both GNOME and KDE entries are ambiguous; neither is a desktop we know. Never guess.
        _ => return Err(PlatformError::Unsupported(UNSUPPORTED_DESKTOP)),
    };
    if is_wayland_session(env) {
        return Ok(desktop);
    }
    let is_x11 = env
        .xdg_session_type
        .as_deref()
        .is_some_and(|session| session.eq_ignore_ascii_case("x11"));
    if is_x11 {
        return Err(PlatformError::Unsupported(X11_SESSION));
    }
    Err(PlatformError::Unsupported(UNSUPPORTED_DESKTOP))
}

/// The error for a Hyprland entry without its IPC signature.
const NO_HYPRLAND_SIGNATURE: &str = "Hyprland without HYPRLAND_INSTANCE_SIGNATURE";
/// The error for GNOME or KDE in an X11 session.
const X11_SESSION: &str = "X11 sessions are not supported; log in to a Wayland session";
/// The error for a desktop Crosspane has no backend for, or an ambiguous one.
const UNSUPPORTED_DESKTOP: &str =
    "unsupported Linux desktop (Crosspane supports Hyprland, GNOME and KDE Plasma on Wayland)";

/// GNOME and KDE need a Wayland session: `XDG_SESSION_TYPE` decides when it is set, otherwise
/// `WAYLAND_DISPLAY` does.
fn is_wayland_session(env: &DesktopEnv) -> bool {
    match env.xdg_session_type.as_deref() {
        Some(session) => session.eq_ignore_ascii_case("wayland"),
        None => env.wayland_display,
    }
}

/// A variable's value when it is set, non-empty and valid UTF-8; anything else counts as unset.
fn non_empty_string(name: &str) -> Option<String> {
    std::env::var_os(name)
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
}

/// Whether a variable is set and non-empty, whatever its bytes.
fn is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment with the given `XDG_CURRENT_DESKTOP` and no Hyprland signature.
    fn desktop_env(desktop: &str, session: Option<&str>, wayland_display: bool) -> DesktopEnv {
        DesktopEnv {
            xdg_current_desktop: Some(desktop.to_owned()),
            xdg_session_type: session.map(str::to_owned),
            hyprland_signature: false,
            wayland_display,
        }
    }

    #[test]
    fn desktop_hyprland_signature_wins() {
        let env = DesktopEnv {
            xdg_current_desktop: Some("GNOME".to_owned()),
            xdg_session_type: Some("x11".to_owned()),
            hyprland_signature: true,
            wayland_display: false,
        };
        assert_eq!(detect(&env).ok(), Some(LinuxDesktop::Hyprland));
    }

    #[test]
    fn desktop_ubuntu_gnome_on_wayland_is_gnome() {
        let env = desktop_env("ubuntu:GNOME", Some("wayland"), false);
        assert_eq!(detect(&env).ok(), Some(LinuxDesktop::Gnome));
    }

    #[test]
    fn desktop_kde_on_wayland_is_kde() {
        let env = desktop_env("KDE", Some("wayland"), false);
        assert_eq!(detect(&env).ok(), Some(LinuxDesktop::Kde));
    }

    #[test]
    fn desktop_gnome_on_x11_is_unsupported() {
        let env = desktop_env("GNOME", Some("x11"), true);
        assert!(matches!(
            detect(&env),
            Err(PlatformError::Unsupported(X11_SESSION))
        ));
    }

    #[test]
    fn desktop_gnome_without_session_type_uses_wayland_display() {
        let env = desktop_env("GNOME", None, true);
        assert_eq!(detect(&env).ok(), Some(LinuxDesktop::Gnome));
    }

    #[test]
    fn desktop_gnome_without_session_or_display_is_unsupported() {
        let env = desktop_env("GNOME", None, false);
        assert!(matches!(
            detect(&env),
            Err(PlatformError::Unsupported(UNSUPPORTED_DESKTOP))
        ));
    }

    #[test]
    fn desktop_hyprland_without_signature_is_unsupported() {
        let env = desktop_env("Hyprland", Some("wayland"), true);
        assert!(matches!(
            detect(&env),
            Err(PlatformError::Unsupported(NO_HYPRLAND_SIGNATURE))
        ));
    }

    #[test]
    fn desktop_gnome_and_kde_are_ambiguous() {
        let env = desktop_env("GNOME:KDE", Some("wayland"), true);
        assert!(matches!(
            detect(&env),
            Err(PlatformError::Unsupported(UNSUPPORTED_DESKTOP))
        ));
    }

    #[test]
    fn desktop_unset_is_unsupported() {
        assert!(matches!(
            detect(&DesktopEnv::default()),
            Err(PlatformError::Unsupported(UNSUPPORTED_DESKTOP))
        ));
    }
}
