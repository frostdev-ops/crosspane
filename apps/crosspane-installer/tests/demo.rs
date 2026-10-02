#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use clap::Parser;
use crosspane_installer::demo::{self, DisconnectedController};
use crosspane_installer::gui::{MAX_FONT_BYTES, ReviewOptions, font_definitions, load_review_font};
use crosspane_installer::*;

fn test_font_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CROSSPANE_TEST_FONT") {
        return PathBuf::from(path);
    }
    const CANDIDATES: [&str; 6] = [
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Verdana.ttf",
    ];
    for candidate in CANDIDATES {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    panic!(
        "Set CROSSPANE_TEST_FONT to a readable system font; none of the candidates exists: {}",
        CANDIDATES.join(", ")
    );
}

fn options(arguments: &[&str]) -> ReviewOptions {
    ReviewOptions::try_parse_from(std::iter::once("installer").chain(arguments.iter().copied()))
        .unwrap()
}

#[test]
fn every_fixture_remains_demo_and_stale_actions_are_rejected() {
    for (screen, _) in demo::SCREENS {
        let mut controller = DisconnectedController::new(Some(screen));
        assert!(controller.view().demo);
        let before = controller.view().clone();
        controller.accept(WizardAction {
            revision: 0,
            intent: WizardIntent::ChooseHiding(HidingChoice::Hide),
        });
        assert_eq!(*controller.view(), before);
        for button in before.buttons {
            controller.accept(WizardAction {
                revision: controller.view().revision,
                intent: WizardIntent::Button(button.id),
            });
            assert!(controller.view().demo);
        }
    }
}

#[test]
fn normal_entry_stays_disconnected_and_cannot_become_a_demo_or_production_job() {
    let mut controller = DisconnectedController::new(None);
    assert_eq!(controller.view().summary, SummaryView::NotInstalled);
    assert!(!controller.view().demo);
    let before = controller.view().clone();
    for intent in [
        WizardIntent::Button(1),
        WizardIntent::Back,
        WizardIntent::ChooseHiding(HidingChoice::Hide),
        WizardIntent::SetToggle {
            field: 21,
            checked: true,
        },
    ] {
        controller.accept(WizardAction {
            revision: 1,
            intent,
        });
        assert_eq!(*controller.view(), before);
    }
    assert!(
        before
            .rows
            .iter()
            .all(|row| row.state != RowState::Verified)
    );
    // The installer core is a dependency since WP-4.4a (its codec); the shell itself still
    // constructs no production port or job, which the assertions above prove.
    let manifest = include_str!("../Cargo.toml");
    for dependency in ["crosspane-agent", "crosspane-platform", "crosspane-ctl"] {
        assert!(!manifest.contains(dependency));
    }
}

#[test]
fn screen_and_screenshot_require_demo_including_ready_summary() {
    for args in [
        vec!["--screen", "summary"],
        vec!["--screenshot", "review.png"],
        vec!["--screen", "summary", "--screenshot", "review.png"],
    ] {
        assert!(options(&args).validate().is_err());
    }
    let valid = options(&[
        "--demo",
        "--screen",
        "summary",
        "--font",
        "/explicit/review.ttf",
        "--screenshot",
        "review.png",
    ]);
    assert_eq!(valid.validate().unwrap(), Some(ScreenId::Summary));
    assert!(
        options(&[
            "--demo",
            "--screen",
            "unknown",
            "--font",
            "/explicit/review.ttf"
        ])
        .validate()
        .is_err()
    );
    assert!(
        ReviewOptions::try_parse_from([
            "installer",
            "--demo",
            "--font",
            "/explicit/review.ttf",
            "--screenshot",
            ""
        ])
        .is_err()
    );
}

#[test]
fn explicit_font_is_required_absolute_nonempty_and_bounded() {
    assert!(
        options(&[])
            .validate()
            .unwrap_err()
            .to_string()
            .contains("--font")
    );
    assert!(
        options(&["--demo", "--font", "relative.ttf"])
            .validate()
            .is_err()
    );
    assert!(font_definitions(Vec::new()).is_err());
    assert!(font_definitions(vec![0; MAX_FONT_BYTES + 1]).is_err());
    assert!(font_definitions(b"not a font".to_vec()).is_err());
    for header in [
        b"OTTO".to_vec(),
        vec![0, 1, 0, 0],
        b"ttcf".to_vec(),
        b"true".to_vec(),
    ] {
        assert!(
            font_definitions(header)
                .unwrap_err()
                .to_string()
                .contains("could not be parsed")
        );
    }
    assert!(load_review_font(&PathBuf::from("relative.ttf")).is_err());
    let definitions = load_review_font(&test_font_path()).unwrap();
    assert_eq!(definitions.font_data.len(), 1);
}

#[test]
fn fresh_consent_defaults_are_explicit_and_no_animation_completes_checks() {
    let permissions = demo::fixture(ScreenId::Permissions);
    for label in [
        "Accessibility",
        "Input Monitoring",
        "Screen Recording",
        "Microphone",
        "Local Network",
    ] {
        assert!(permissions.rows.iter().any(|row| row.label == label));
    }
    assert_eq!(demo::fixture(ScreenId::HidingChoice).hiding_choice, None);
    let view = demo::fixture(ScreenId::RepairRemove);
    assert!(view.fields.iter().any(|field| matches!(field, FieldView::Toggle { role: ToggleRole::RemoveAudioDriver, checked: true, label, .. } if label == demo::REMOVE_AUDIO_LABEL)));
    assert!(view.fields.iter().any(|field| matches!(
        field,
        FieldView::Toggle {
            role: ToggleRole::DeleteIdentity,
            checked: false,
            ..
        }
    )));
    let mut controller = DisconnectedController::new(Some(ScreenId::Installing));
    let rows = controller.view().rows.clone();
    controller.accept(WizardAction {
        revision: 1,
        intent: WizardIntent::Button(5),
    });
    assert_eq!(controller.view().rows, rows);
    assert_eq!(controller.view().summary, SummaryView::NotInstalled);
}

#[test]
fn font_directories_are_rejected_before_reading() {
    let directory = std::env::temp_dir();
    assert!(
        load_review_font(&directory)
            .unwrap_err()
            .to_string()
            .contains("regular file")
    );
}
