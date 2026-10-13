//! Crosspane installer and setup guide (WP-4.3 onwards).
//!
//! Skeleton registered by WP-4.0; the implementation lands in its own work package.

#![cfg_attr(windows, windows_subsystem = "windows")]

use clap::Parser;
#[cfg(windows)]
use crosspane_installer::gui::LaunchMode;
use crosspane_installer::gui::{ReviewOptions, run};
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
    // Windows: the supervisor, the headless lifecycle commands and `--maintenance` are decided
    // before any GUI work. Everything else (including `--diagnose`) falls through to the GUI path.
    #[cfg(windows)]
    let maintenance = {
        use crosspane_installer::platform::windows::{cli, request_maintenance, supervisor};
        let arguments: Vec<_> = std::env::args_os().skip(1).collect();
        match cli::parse(&arguments) {
            Ok(cli::Launch::Supervisor) => {
                platform::windows::setup_log::setup_note!("supervisor start");
                let code = supervisor::run();
                platform::windows::setup_log::setup_note!("supervisor exit: {code}");
                std::process::exit(code);
            }
            Ok(cli::Launch::Cli(command)) => std::process::exit(cli::run(command)),
            Ok(cli::Launch::Maintenance) => {
                request_maintenance();
                true
            }
            Ok(cli::Launch::Gui) => false,
            Err(message) => std::process::exit(cli::usage_exit(&message)),
        }
    };
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
    // `--maintenance` is only valid alone, so the GUI options are then the defaults.
    #[cfg(windows)]
    let parsed = if maintenance {
        ReviewOptions::try_parse_from(["crosspane-installer"])
    } else {
        ReviewOptions::try_parse()
    };
    #[cfg(not(windows))]
    let parsed = ReviewOptions::try_parse();
    let options = match parsed {
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
