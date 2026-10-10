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
        todo_body()
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
    let _ = env;
    Err(PlatformError::Unsupported(
        "desktop detection not implemented yet",
    ))
}

fn todo_body() -> DesktopEnv {
    DesktopEnv::default()
}
