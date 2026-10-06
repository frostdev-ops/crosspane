//! Pure shell regressions: no native install/agent constructor or GUI is invoked.
#![allow(clippy::unwrap_used, clippy::expect_used)]

pub use crosspane_installer::{agent_contract, demo, gui, live, view};

#[allow(dead_code, unused_imports)]
#[path = "../src/platform/windows/mod.rs"]
mod shell;

use clap::Parser;
use crosspane_installer::view::{RowState, ScreenId, WizardAction, WizardIntent};
use std::sync::Arc;

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

#[test]
fn windows_first_dependent_action_ends_on_existing_failure_screen() {
    let mut controller = shell::unavailable(Arc::new(|| 1_000)).unwrap();
    assert!(!controller.accept(WizardAction {
        revision: controller.view().revision,
        intent: WizardIntent::Button(live::ids::NEXT),
    }));
    controller.tick();
    let view = controller.view();
    assert_eq!(view.screen, ScreenId::Compatibility);
    assert_eq!(view.title, "Setup stopped");
    assert!(view.rows.iter().any(|row| {
        row.state == RowState::Failed
            && row
                .detail
                .contains("Windows install isn't available in this build yet")
    }));
    assert!(!view.rows.iter().any(|row| row.state == RowState::Working));
    controller.close();
}

#[test]
fn windows_unavailable_failure_does_not_turn_into_a_spinner_on_tick() {
    let mut controller = shell::unavailable(Arc::new(|| 1_000)).unwrap();
    controller.accept(WizardAction {
        revision: controller.view().revision,
        intent: WizardIntent::Button(live::ids::NEXT),
    });
    for _ in 0..32 {
        controller.tick();
    }
    assert!(
        controller
            .view()
            .rows
            .iter()
            .any(|row| row.state == RowState::Failed)
    );
    assert!(
        !controller
            .view()
            .rows
            .iter()
            .any(|row| row.state == RowState::Working)
    );
    assert!(controller.request_close());
}
