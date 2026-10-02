//! Crosspane installer and setup guide (WP-4.3 onwards).
//!
//! Skeleton registered by WP-4.0; the implementation lands in its own work package.

use clap::Parser;
use crosspane_installer::gui::{ReviewOptions, run};
use crosspane_ui_kit::art::BrandBytes;

fn main() {
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
