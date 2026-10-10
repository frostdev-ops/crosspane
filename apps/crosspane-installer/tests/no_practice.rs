#![allow(clippy::unwrap_used)]
//! Focused disconnected flow/rail regressions for WP-4.36; no native ports or services.
use crosspane_installer::{
    demo::{self, DisconnectedController},
    live::ids,
    view::{RowState, ScreenId, WizardAction, WizardIntent},
};
use crosspane_installer_core::{Flow, GraphError, StepId, StepSpec};

#[test]
fn remaining_demo_screens_and_variants_have_no_practice() {
    assert_eq!(
        demo::SCREENS
            .iter()
            .map(|(_, name)| *name)
            .collect::<Vec<_>>(),
        [
            "welcome",
            "compatibility",
            "install-plan",
            "installing",
            "permissions",
            "audio-component",
            "network",
            "hiding-choice",
            "connect",
            "match-numbers",
            "grants",
            "layout",
            "summary",
            "repair-remove",
        ]
    );
    assert_eq!(demo::screen_named("practice"), None);
    assert_eq!(demo::screen_named("practice-choose"), None);
    for (screen, _) in demo::SCREENS {
        let view = demo::fixture(screen);
        assert!(view.demo);
        assert!(!format!("{:?}", view.progress).contains("Practice"));
    }
}

#[test]
fn arrange_demo_proceeds_directly_to_ready() {
    let mut controller = DisconnectedController::new(Some(ScreenId::Layout));
    controller.accept(WizardAction {
        revision: controller.view().revision,
        intent: WizardIntent::Button(ids::LAYOUT_ACCEPT),
    });
    assert_eq!(controller.view().screen, ScreenId::Summary);
    assert!(controller.view().demo);
}

#[test]
fn ready_lists_only_the_existing_connect_and_arrange_deferrals() {
    let view = demo::fixture_named("summary-skipped").unwrap();
    let rows: Vec<_> = view
        .rows
        .iter()
        .filter(|row| row.state == RowState::Skipped)
        .map(|row| (row.id, row.label.as_str(), row.detail.as_str()))
        .collect();
    let detail = if cfg!(windows) {
        "Do this later from Crosspane > Settings; open Crosspane from the Start menu"
    } else {
        "Do this later from the Crosspane menu > Settings"
    };
    assert_eq!(
        rows,
        [
            (940, "Connect another computer", detail),
            (941, "Arrange your screens", detail),
        ]
    );
    assert_eq!(view.screen, ScreenId::Summary);
    assert!(view.demo);
}

fn step(id: u16) -> StepSpec {
    StepSpec {
        id: StepId(id),
        prerequisites: vec![],
        required_for_installed: false,
        required_for_ready: true,
        requires_fresh_observation: id == 90,
        requires_activity: false,
        requires_human: false,
        requires_fixture: false,
    }
}

#[test]
fn core_optional_ids_remain_connect_and_arrange_only() {
    let mut graph: Vec<_> = [60, 61, 62, 90].into_iter().map(step).collect();
    graph.last_mut().unwrap().prerequisites = vec![StepId(60), StepId(61), StepId(62)];
    assert!(Flow::new_with_optional_steps(graph, &[StepId(60), StepId(61), StepId(62)]).is_ok());
    for legacy in 70..=78 {
        let mut final_step = step(90);
        final_step.prerequisites.push(StepId(legacy));
        assert_eq!(
            Flow::new_with_optional_steps(vec![step(legacy), final_step], &[StepId(legacy)])
                .unwrap_err(),
            GraphError::InvalidOptionalStep,
        );
    }
}
