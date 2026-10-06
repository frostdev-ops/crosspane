//! Crosspane installer and setup guide (WP-4.3 onwards).
//!
//! Skeleton registered by WP-4.0; the implementation lands in its own work package.

use clap::Parser;
use crosspane_installer::gui::{ReviewOptions, run};
use crosspane_ui_kit::art::BrandBytes;

#[derive(Parser)]
struct DiagnoseOptions {
    #[arg(long)]
    diagnose: bool,
    #[arg(long)]
    payload: Option<std::path::PathBuf>,
}
fn main() {
    #[cfg(windows)]
    {
        let arguments: Vec<_> = std::env::args_os().skip(1).collect();
        // The fixed helper entry is admitted before any supervisor, diagnose or GUI work.
        match crosspane_installer::platform::windows::replace_helper_mode(&arguments) {
            Ok(true) => {
                if let Err(error) = crosspane_installer::platform::windows::replace_helper_entry() {
                    eprintln!("Crosspane Installer replace helper: {error}");
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
                if let Err(error) =
                    crosspane_installer::platform::windows::service::supervisor_entry()
                {
                    eprintln!("Crosspane Installer supervisor: {error}");
                    std::process::exit(1);
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
        let options = DiagnoseOptions::parse();
        if let Err(error) = crosspane_installer::diagnose::run(options.payload) {
            eprintln!("Crosspane Installer diagnose: {error}");
            std::process::exit(1);
        }
        return;
    }
    let options = ReviewOptions::parse();
    let brand = BrandBytes {
        backdrop: include_bytes!("../../crosspane-ui/assets/backdrop.png"),
        emblem: include_bytes!("../../crosspane-ui/assets/emblem-256.png"),
        wordmark: include_bytes!("../../crosspane-ui/assets/wordmark.png"),
    };
    if let Err(error) = run(options, brand) {
        eprintln!("Crosspane Installer: {error:#}");
        std::process::exit(1);
    }
}
