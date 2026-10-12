//! Crosspane installer and setup guide (WP-4.3 onwards).
//!
//! Skeleton registered by WP-4.0; the implementation lands in its own work package.

#![cfg_attr(windows, windows_subsystem = "windows")]

use clap::Parser;
use crosspane_installer::gui::{LaunchMode, ReviewOptions, run};
use crosspane_ui_kit::art::BrandBytes;

#[derive(Parser)]
struct DiagnoseOptions {
    #[arg(long)]
    diagnose: bool,
    #[arg(long)]
    payload: Option<std::path::PathBuf>,
}

/// Console and message-box helpers for the GUI-subsystem binary.
#[cfg(windows)]
mod console {
    use windows_sys::Win32::{
        System::Console::{ATTACH_PARENT_PROCESS, AttachConsole},
        UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
    };

    /// Attaches to the parent console, if there is one, so terminal output stays visible.
    pub fn attach_parent() {
        // SAFETY: AttachConsole takes no pointers. Failure means there is no parent console
        // (for example Explorer), which is expected and ignored.
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }

    /// Shows a modal error box titled "Crosspane Setup" with `text`.
    pub fn error_box(text: &str) {
        let text = wide(text);
        let caption = wide("Crosspane Setup");
        // SAFETY: both pointers are NUL-terminated UTF-16 buffers that live until the call
        // returns. A null owner window is valid for MessageBoxW.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                caption.as_ptr(),
                MB_OK | MB_ICONERROR,
            )
        };
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

/// The library's setup log, included by path: this binary is a separate crate, and the library's
/// copy is `pub(crate)`. The path is relative to this file's directory, so `setup_note!` resolves
/// through `$crate::platform::windows::setup_log` exactly as it does in the library.
#[cfg(windows)]
mod platform {
    pub(crate) mod windows {
        #[path = "setup_log.rs"]
        pub(crate) mod setup_log;
    }
}

fn main() {
    #[cfg(windows)]
    {
        let arguments: Vec<_> = std::env::args_os().skip(1).collect();
        // A4d keeper has one exact flag and no path/PID/operation argv authority.
        match crosspane_installer::platform::windows::service::upgrade_keeper_mode(&arguments) {
            Ok(true) => {
                if let Err(error) =
                    crosspane_installer::platform::windows::service::upgrade_keeper_entry()
                {
                    eprintln!("Crosspane Installer upgrade keeper: {error}");
                    platform::windows::setup_log::setup_note!("upgrade keeper failed: {error:?}");
                    std::process::exit(1);
                }
                return;
            }
            Err(_) => {
                eprintln!("Crosspane Installer upgrade keeper: invalid arguments");
                std::process::exit(2);
            }
            Ok(false) => {}
        }
        // The fixed helper entry is admitted before any supervisor, diagnose or GUI work.
        match crosspane_installer::platform::windows::replace_helper_mode(&arguments) {
            Ok(true) => {
                if let Err(error) = crosspane_installer::platform::windows::replace_helper_entry() {
                    eprintln!("Crosspane Installer replace helper: {error}");
                    platform::windows::setup_log::setup_note!("replace helper failed: {error:?}");
                    std::process::exit(1);
                }
                return;
            }
            Err(_) => {
                eprintln!("Crosspane Installer replace helper: invalid arguments");
                std::process::exit(2);
            }
            Ok(false) => {}
        }
        match crosspane_installer::platform::windows::service::supervisor_mode(&arguments) {
            Ok(true) => {
                platform::windows::setup_log::setup_note!("supervisor start");
                match crosspane_installer::platform::windows::service::supervisor_entry() {
                    Ok(exit) => {
                        platform::windows::setup_log::setup_note!("supervisor exit: {exit:?}");
                    }
                    Err(error) => {
                        eprintln!("Crosspane Installer supervisor: {error}");
                        platform::windows::setup_log::setup_note!("supervisor failed: {error:?}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            Err(_) => {
                eprintln!("Crosspane Installer supervisor: invalid arguments");
                std::process::exit(2);
            }
            Ok(false) => {}
        }
    }
    if std::env::args_os().any(|arg| arg == "--diagnose") {
        #[cfg(windows)]
        console::attach_parent();
        let options = DiagnoseOptions::parse();
        if let Err(error) = crosspane_installer::diagnose::run(options.payload) {
            eprintln!("Crosspane Installer diagnose: {error}");
            std::process::exit(1);
        }
        return;
    }
    let options = match ReviewOptions::try_parse() {
        Ok(options) => options,
        Err(error) => {
            #[cfg(windows)]
            console::attach_parent();
            error.exit()
        }
    };
    let brand = BrandBytes {
        backdrop: include_bytes!("../../crosspane-ui/assets/backdrop.png"),
        emblem: include_bytes!("../../crosspane-ui/assets/emblem-256.png"),
        wordmark: include_bytes!("../../crosspane-ui/assets/wordmark.png"),
    };
    #[cfg(windows)]
    let production = options.mode() == LaunchMode::Production;
    if let Err(error) = run(options, brand) {
        #[cfg(windows)]
        if !production {
            console::attach_parent();
        }
        eprintln!("Crosspane Installer: {error:#}");
        #[cfg(windows)]
        if production {
            platform::windows::setup_log::setup_note!("startup error: {error:#}");
            console::error_box(&format!("{error:#}"));
        }
        std::process::exit(1);
    }
}
