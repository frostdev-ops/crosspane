//! The per-session daemon wiring engine, platform, and transport.

use clap::{Parser, Subcommand};
use crosspane_platform::{Permission, PermissionState, Permissions};

#[derive(Debug, Parser)]
#[command(
    name = "crosspane-agent",
    version,
    about = "Crosspane per-session agent"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the OS permission status as one JSON line.
    Permissions {
        /// Start the OS's grant flow for every permission not yet granted.
        #[arg(long)]
        request: bool,
        /// Also open the System Settings pane for every permission not yet granted (macOS).
        #[arg(long)]
        open_settings: bool,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    if crosspane_platform_is_elevated() {
        anyhow::bail!("crosspane-agent refuses to run elevated (04 §7)");
    }
    match Cli::parse().command {
        Some(Command::Permissions {
            request,
            open_settings,
        }) => permissions(request, open_settings),
        None => anyhow::bail!("the agent service is not wired yet (WP-1.37); see --help"),
    }
}

fn permissions(request: bool, open_settings: bool) -> anyhow::Result<()> {
    let mut perms = platform_permissions();
    let mut report = serde_json::Map::new();
    for permission in perms.required() {
        let mut state = perms.state(permission);
        if state != PermissionState::Granted {
            if request {
                perms.request(permission)?;
            }
            if open_settings {
                open_settings_pane(permission);
            }
            state = perms.state(permission);
        }
        report.insert(permission_name(permission).into(), state_name(state).into());
    }
    println!("{}", serde_json::Value::Object(report));
    Ok(())
}

fn permission_name(permission: Permission) -> &'static str {
    match permission {
        Permission::ScreenRecording => "screen_recording",
        Permission::Accessibility => "accessibility",
        Permission::InputMonitoring => "input_monitoring",
        _ => "other",
    }
}

fn state_name(state: PermissionState) -> &'static str {
    match state {
        PermissionState::Granted => "granted",
        PermissionState::NotGranted => "not_granted",
        PermissionState::Unknown => "unknown",
    }
}

#[cfg(target_os = "macos")]
fn platform_permissions() -> Box<dyn Permissions> {
    Box::new(crosspane_platform_macos::permissions::MacPermissions::new())
}

#[cfg(target_os = "linux")]
fn platform_permissions() -> Box<dyn Permissions> {
    Box::new(crosspane_platform_linux::permissions::LinuxPermissions)
}

#[cfg(target_os = "macos")]
fn open_settings_pane(permission: Permission) {
    let url = crosspane_platform_macos::permissions::MacPermissions::settings_url(permission);
    if let Err(e) = std::process::Command::new("/usr/bin/open")
        .arg(url)
        .status()
    {
        tracing::warn!(error = %e, "could not open System Settings");
    }
}

#[cfg(not(target_os = "macos"))]
fn open_settings_pane(_permission: Permission) {}

/// True when running as root (uid 0), which the agent refuses (04 §7).
fn crosspane_platform_is_elevated() -> bool {
    #[cfg(unix)]
    {
        // `id -u` avoids a libc dependency for one check at startup.
        std::process::Command::new("/usr/bin/id")
            .arg("-u")
            .output()
            .map(|o| o.stdout.starts_with(b"0\n"))
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        false
    }
}
