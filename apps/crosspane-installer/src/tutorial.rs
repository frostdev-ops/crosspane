//! Controlled practice child. Stdout is protocol-only except for explicit --help.
use clap::{Parser, error::ErrorKind};
use crosspane_installer::tutorial_window::{TutorialOptions, run};
fn usage() -> ! {
    eprintln!("crosspane-tutorial: usage: --controlled --font <absolute-path>");
    std::process::exit(2);
}
fn main() {
    std::panic::set_hook(Box::new(|_| eprintln!("crosspane-tutorial: panic")));
    let options = match TutorialOptions::try_parse() {
        Ok(options) => options,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            print!("{error}");
            return;
        }
        Err(_) => usage(),
    };
    if options.validate().is_err() {
        usage();
    }
    if run(options).is_err() {
        eprintln!("crosspane-tutorial: unavailable");
        std::process::exit(1);
    }
}
