//! Crosspane one-time elevated setup helper (WP-W4.1c). Windows only: idempotent, verb-scoped
//! operations on Crosspane's own firewall rule, IddCx driver package and adapter node.

#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
fn main() {
    eprintln!("crosspane-elevated-setup: Windows only");
    std::process::exit(crosspane_installer_core::elevated::Outcome::Refused.exit_code());
}

#[cfg(windows)]
fn main() {
    let arguments: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    std::process::exit(windows::entry(&arguments));
}
