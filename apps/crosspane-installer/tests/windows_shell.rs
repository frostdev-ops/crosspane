//! Pure shell regressions: no native install/agent constructor or GUI is invoked.
#![allow(clippy::unwrap_used, clippy::expect_used)]

pub use crosspane_installer::{agent_contract, demo, gui, live, view};

#[allow(dead_code, unused_imports)]
#[path = "../src/platform/windows/mod.rs"]
mod shell;

use clap::Parser;

fn options(extra: &[&str]) -> gui::ReviewOptions {
    let font = if cfg!(windows) {
        r"C:\fixture\font.ttf"
    } else {
        "/fixture/font.ttf"
    };
    gui::ReviewOptions::try_parse_from(
        ["crosspane-installer", "--font", font]
            .into_iter()
            .chain(extra.iter().copied()),
    )
    .unwrap()
}

#[test]
fn own_demo_close_flag_accepts_a_bounded_windowed_run() {
    assert!(
        options(&["--demo", "--exit-after-ms", "3000"])
            .validate()
            .is_ok()
    );
}

#[test]
fn own_demo_close_flag_cannot_be_used_for_production_or_offscreen() {
    assert!(options(&["--exit-after-ms", "3000"]).validate().is_err());
    assert!(
        options(&[
            "--demo",
            "--offscreen",
            "--screenshot",
            "own.png",
            "--exit-after-ms",
            "3000"
        ])
        .validate()
        .is_err()
    );
}

#[test]
fn own_demo_close_flag_rejects_zero_and_over_eight_seconds() {
    assert!(
        options(&["--demo", "--exit-after-ms", "0"])
            .validate()
            .is_err()
    );
    assert!(
        options(&["--demo", "--exit-after-ms", "8001"])
            .validate()
            .is_err()
    );
    assert!(
        options(&["--demo", "--exit-after-ms", "8000"])
            .validate()
            .is_ok()
    );
}
