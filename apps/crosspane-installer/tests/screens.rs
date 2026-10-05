#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use crosspane_installer::demo;
use crosspane_installer::gui::load_review_font;
use crosspane_installer::*;
use crosspane_ui_kit::{
    art::{Art, BrandBytes},
    theme,
};
use eframe::egui::{self, Event, Key, Modifiers, PointerButton, Pos2, Rect};

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

struct Harness {
    ctx: egui::Context,
    shell: WizardShell,
    art: Art,
    size: egui::Vec2,
    now: u64,
    output: Option<egui::FullOutput>,
    focused_rect: Option<Rect>,
    margin: f32,
}

impl Harness {
    fn new(size: egui::Vec2) -> Self {
        let ctx = egui::Context::default();
        ctx.set_fonts(load_review_font(&test_font_path()).unwrap());
        ctx.set_theme(egui::Theme::Dark);
        ctx.set_style_of(egui::Theme::Dark, theme::style());
        let art = Art::load(
            &ctx,
            BrandBytes {
                backdrop: &[],
                emblem: &[],
                wordmark: &[],
            },
        );
        Self {
            ctx,
            shell: WizardShell::default(),
            art,
            size,
            now: 0,
            output: None,
            focused_rect: None,
            margin: 8.0,
        }
    }

    fn frame(&mut self, view: &WizardView, events: Vec<Event>) -> Vec<WizardAction> {
        self.now += 50;
        let mut actions = Vec::new();
        let mut focused_rect = None;
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)),
            time: Some(self.now as f64 / 1000.0),
            events,
            ..Default::default()
        };
        let mut output = self.ctx.run_ui(input, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.inner_margin(self.margin))
                .show(ui, |ui| {
                    actions = self.shell.show(ui, view, &self.art, self.now);
                    focused_rect = ui
                        .memory(|memory| memory.focused())
                        .and_then(|id| ui.ctx().read_response(id))
                        .map(|response| response.rect);
                });
        });
        // This CPU-only harness intentionally has no texture renderer.
        output.textures_delta.clear();
        self.output = Some(output);
        self.focused_rect = focused_rect;
        actions
    }

    fn settle(&mut self, view: &WizardView) {
        for _ in 0..8 {
            assert!(self.frame(view, Vec::new()).is_empty());
        }
    }

    fn review(size: egui::Vec2) -> Self {
        let mut harness = Self::new(size);
        harness.margin = 24.0;
        harness
    }

    fn shapes(&self) -> Vec<(&egui::epaint::Shape, Rect)> {
        fn visit<'a>(
            shape: &'a egui::epaint::Shape,
            clip: Rect,
            values: &mut Vec<(&'a egui::epaint::Shape, Rect)>,
        ) {
            match shape {
                egui::epaint::Shape::Vec(children) => {
                    for child in children {
                        visit(child, clip, values);
                    }
                }
                _ => values.push((shape, clip)),
            }
        }
        let mut values = Vec::new();
        for clipped in &self.output.as_ref().unwrap().shapes {
            visit(&clipped.shape, clipped.clip_rect, &mut values);
        }
        values
    }

    fn raw_text(&self, label: &str) -> (Rect, Rect, &egui::epaint::TextShape) {
        self.shapes()
            .into_iter()
            .find_map(|(shape, clip)| match shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                    Some((text.galley.rect.translate(text.pos.to_vec2()), clip, text))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("Missing raw fixture label {label:?}"))
    }

    fn card(&self, label: &str) -> (Rect, Rect) {
        let label_rect = self.raw_text(label).0;
        self.shapes()
            .into_iter()
            .filter_map(|(shape, clip)| match shape {
                egui::epaint::Shape::Rect(rect)
                    if rect.fill == theme::alpha(theme::MIDNIGHT, 205)
                        && rect.stroke.color == theme::alpha(theme::GLACIER, 42)
                        && rect.rect.contains(label_rect.center()) =>
                {
                    Some((rect.rect, clip))
                }
                _ => None,
            })
            .min_by(|(a, _), (b, _)| a.area().total_cmp(&b.area()))
            .unwrap()
    }

    fn texts(&self) -> Vec<(String, Rect)> {
        fn visit(shape: &egui::epaint::Shape, clip: Rect, values: &mut Vec<(String, Rect)>) {
            match shape {
                egui::epaint::Shape::Text(text) => {
                    let rect = text
                        .galley
                        .rect
                        .translate(text.pos.to_vec2())
                        .intersect(clip);
                    if rect.is_positive() {
                        values.push((text.galley.text().into(), rect));
                    }
                }
                egui::epaint::Shape::Vec(children) => {
                    for child in children {
                        visit(child, clip, values);
                    }
                }
                _ => {}
            }
        }
        let mut values = Vec::new();
        for clipped in &self.output.as_ref().unwrap().shapes {
            visit(&clipped.shape, clipped.clip_rect, &mut values);
        }
        values
    }

    fn rect(&self, label: &str) -> Rect {
        self.texts()
            .into_iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| {
                panic!(
                    "Missing visible label {label:?}; visible: {:?}",
                    self.texts()
                )
            })
            .1
    }

    fn click(&mut self, view: &WizardView, label: &str) -> Vec<WizardAction> {
        let point = self.rect(label).center();
        self.frame(view, vec![Event::PointerMoved(point)]);
        self.frame(view, vec![pointer(point, true)]);
        self.frame(view, vec![pointer(point, false)])
    }

    fn focused_label(&self, label: &str) -> bool {
        self.focused_rect
            .is_some_and(|rect| rect.contains(self.rect(label).center()))
    }

    fn focus_label(&mut self, view: &WizardView, label: &str) {
        for _ in 0..32 {
            self.frame(view, vec![key(Key::Tab, false)]);
            if self.focused_label(label) {
                return;
            }
        }
        panic!("Tab did not reach {label}");
    }
}

fn pointer(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    }
}

fn key(key: Key, shift: bool) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers {
            shift,
            ..Modifiers::NONE
        },
    }
}

#[test]
fn all_fifteen_screens_render_with_demo_label_and_supplied_state_at_both_sizes() {
    for size in [egui::vec2(1100.0, 760.0), egui::vec2(800.0, 600.0)] {
        let mut harness = Harness::new(size);
        for (screen, _) in demo::SCREENS {
            let view = demo::fixture(screen);
            let before = view.clone();
            harness.settle(&view);
            assert!(
                harness
                    .texts()
                    .iter()
                    .any(|(text, _)| text == demo::DEMO_LABEL)
            );
            assert!(harness.texts().iter().any(|(text, _)| text == &view.title));
            assert_eq!(view, before);
            for button in &view.buttons {
                assert!(
                    harness.rect(&button.label).max.y <= size.y,
                    "Footer escaped viewport on {screen:?}"
                );
            }
        }
    }
}

#[test]
fn enabled_disabled_buttons_emit_only_rendered_revision() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::Welcome);
    view.revision = 42;
    view.buttons[0].enabled = false;
    harness.settle(&view);
    assert!(harness.click(&view, "Continue").is_empty());
    view.buttons[0].enabled = true;
    view.revision = 43;
    harness.settle(&view);
    assert_eq!(
        harness.click(&view, "Continue"),
        vec![WizardAction {
            revision: 43,
            intent: WizardIntent::Button(1)
        }]
    );
}

#[test]
fn hiding_is_unselected_and_next_role_is_gated_without_label_guessing() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::HidingChoice);
    view.buttons[1].label = "Translated next".into();
    harness.settle(&view);
    assert_eq!(view.hiding_choice, None);
    assert!(harness.click(&view, "Translated next").is_empty());
    assert_eq!(
        harness.click(&view, demo::HIDE_LABEL),
        vec![WizardAction {
            revision: 1,
            intent: WizardIntent::ChooseHiding(HidingChoice::Hide)
        }]
    );
    assert_eq!(view.hiding_choice, None);
    view.hiding_choice = Some(HidingChoice::Hide);
    view.revision += 1;
    harness.settle(&view);
    assert_eq!(
        harness.click(&view, "Translated next"),
        vec![WizardAction {
            revision: 2,
            intent: WizardIntent::Button(1)
        }]
    );
    assert_eq!(
        harness.click(&view, demo::MIRROR_LABEL),
        vec![WizardAction {
            revision: 2,
            intent: WizardIntent::ChooseHiding(HidingChoice::Mirror)
        }]
    );
}

#[test]
fn literal_privacy_copy_and_global_removal_checkbox_are_present() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let view = demo::fixture(ScreenId::AudioComponent);
    harness.settle(&view);
    assert_eq!(
        harness
            .shapes()
            .into_iter()
            .filter(|(shape, _)| {
                matches!(shape, egui::epaint::Shape::Text(text)
                    if text.galley.text() == demo::MICROPHONE_DETAIL
                        && text.galley.text().contains(demo::MICROPHONE_COPY))
            })
            .count(),
        1
    );
    let view = demo::fixture(ScreenId::RepairRemove);
    harness.settle(&view);
    assert_eq!(
        harness.click(&view, demo::REMOVE_AUDIO_LABEL),
        vec![WizardAction {
            revision: 1,
            intent: WizardIntent::SetToggle {
                field: 21,
                checked: false
            }
        }]
    );
    let label = "Delete this machine's Crosspane identity and trust";
    assert_eq!(
        harness.click(&view, label),
        vec![WizardAction {
            revision: 1,
            intent: WizardIntent::SetToggle {
                field: 20,
                checked: true
            }
        }]
    );
}

#[test]
fn address_edit_is_single_line_bounded_and_passive_updates_retain_focus() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::Connect);
    // Keep the address visible; the unrelated waiting row is supplied independently.
    view.rows.clear();
    harness.settle(&view);
    harness.click(&view, "Address of the other machine");
    let focused = harness.ctx.memory(|memory| memory.focused());
    let actions = harness.frame(&view, vec![Event::Text("peer.example".into())]);
    assert_eq!(
        actions,
        vec![WizardAction {
            revision: 1,
            intent: WizardIntent::EditPeerAddress {
                field: 10,
                value: "peer.example".into()
            }
        }]
    );
    if let FieldView::PeerAddress { value, .. } = &mut view.fields[0] {
        *value = "peer.example".into();
    }
    view.message = "A passive discovery update".into();
    harness.frame(&view, Vec::new());
    assert_eq!(harness.ctx.memory(|memory| memory.focused()), focused);
    let actions = harness.frame(&view, vec![Event::Paste("é".repeat(400))]);
    assert!(
        matches!(&actions[..], [WizardAction { intent: WizardIntent::EditPeerAddress { value, .. }, .. }] if value.len() <= 512 && value.is_char_boundary(value.len()) && !value.contains(['\r', '\n']))
    );
}

#[test]
fn tab_shift_tab_focused_enter_and_explicit_escape_mapping() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::Welcome);
    harness.settle(&view);
    assert!(
        harness
            .frame(&view, vec![key(Key::Enter, false)])
            .is_empty()
    );
    harness.focus_label(&view, "Continue");
    let focused = harness.ctx.memory(|memory| memory.focused()).unwrap();
    harness.frame(&view, vec![key(Key::Tab, false)]);
    let next = harness.ctx.memory(|memory| memory.focused()).unwrap();
    assert_ne!(next, focused);
    harness.frame(&view, vec![key(Key::Tab, true)]);
    harness.frame(&view, Vec::new());
    assert_eq!(harness.ctx.memory(|memory| memory.focused()), Some(focused));
    assert_eq!(
        harness.frame(&view, vec![key(Key::Enter, false)]),
        vec![WizardAction {
            revision: 1,
            intent: WizardIntent::Button(1)
        }]
    );
    for (mapping, expected) in [
        (EscapeMapping::None, None),
        (EscapeMapping::Back, Some(WizardIntent::Back)),
        (EscapeMapping::Close, Some(WizardIntent::Close)),
    ] {
        view.escape = mapping;
        let actions = harness.frame(&view, vec![key(Key::Escape, false)]);
        assert_eq!(
            actions,
            expected
                .into_iter()
                .map(|intent| WizardAction {
                    revision: 1,
                    intent
                })
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn changed_plan_clears_button_focus_and_discards_queued_enter() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::Welcome);
    harness.settle(&view);
    harness.focus_label(&view, "Continue");
    let old = harness.ctx.memory(|memory| memory.focused()).unwrap();
    view.revision = 2;
    view.buttons[0].label = "Remove a different target".into();
    view.buttons[0].kind = ButtonKind::Destructive;
    assert!(
        harness
            .frame(&view, vec![key(Key::Enter, false)])
            .is_empty()
    );
    assert_ne!(harness.ctx.memory(|memory| memory.focused()), Some(old));
    harness.focus_label(&view, "Remove a different target");
    assert_eq!(
        harness.frame(&view, vec![key(Key::Enter, false)]),
        vec![WizardAction {
            revision: 2,
            intent: WizardIntent::Button(1)
        }]
    );
}

#[test]
fn disabled_controls_are_skipped_and_choices_are_keyboard_reachable() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::HidingChoice);
    view.buttons[0].enabled = false;
    harness.settle(&view);
    harness.focus_label(&view, demo::MIRROR_LABEL);
    assert_eq!(
        harness.frame(&view, vec![key(Key::Enter, false)]),
        vec![WizardAction {
            revision: 1,
            intent: WizardIntent::ChooseHiding(HidingChoice::Mirror)
        }]
    );
    for _ in 0..16 {
        harness.frame(&view, vec![key(Key::Tab, false)]);
        assert!(!harness.focused_label("Continue"));
        assert!(!harness.focused_label("Back"));
    }
}

#[test]
fn keyboard_scrolls_long_content_while_release_and_stop_remain_visible() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::Practice);
    view.rows = (0..40)
        .map(|id| RowView {
            id,
            label: format!("Practice result {id}"),
            detail: "Waiting for evidence. ".repeat(12),
            state: RowState::Waiting,
            human_confirmed: false,
        })
        .collect();
    harness.settle(&view);
    let before = harness.texts();
    // The focusable scroll area precedes the footer in Tab order.
    harness.frame(&view, vec![key(Key::Tab, false)]);
    harness.frame(&view, vec![key(Key::PageDown, false)]);
    harness.settle(&view);
    let after = harness.texts();
    assert_ne!(before, after);
    assert!(harness.rect("Release control").max.y <= 600.0);
    assert!(harness.rect("Stop practice").max.y <= 600.0);
    assert_eq!(
        harness.click(&view, "Release control"),
        vec![WizardAction {
            revision: 1,
            intent: WizardIntent::Button(6)
        }]
    );
}

#[test]
fn layout_reconciliation_is_available_offscreen_and_revert_clears_edits() {
    let mut harness = Harness::new(egui::vec2(1100.0, 760.0));
    let mut view = demo::fixture(ScreenId::Layout);
    harness.settle(&view);
    let peer = harness.rect("Other machine").center();
    harness.frame(&view, vec![Event::PointerMoved(peer)]);
    harness.frame(&view, vec![pointer(peer, true)]);
    let moved = peer + egui::vec2(70.0, 24.0);
    harness.frame(&view, vec![Event::PointerMoved(moved)]);
    harness.frame(&view, vec![pointer(moved, false)]);
    harness.settle(&view);
    let edited_position = harness.rect("Other machine");
    let mut newer = view.layout.as_ref().unwrap().confirmed.clone();
    newer[1].origin = [650.0, 0.0];
    let welcome = demo::fixture(ScreenId::Welcome);
    harness.settle(&welcome);
    harness.shell.follow_layout(&newer);
    view.layout.as_mut().unwrap().confirmed = newer.clone();
    harness.settle(&view);
    assert_eq!(
        harness.rect("Other machine"),
        edited_position,
        "follow must preserve an uncommitted edit"
    );
    harness.settle(&welcome);
    harness.shell.revert_layout(&newer);
    harness.settle(&view);
    assert_ne!(harness.rect("Other machine"), edited_position);
    assert!(
        harness.click(&view, "Apply").is_empty(),
        "post-Place acknowledgement must clear old edits"
    );
}

#[test]
fn layout_drag_cancellation_preserves_edits_and_stops_pointer_motion() {
    let mut harness = Harness::new(egui::vec2(1100.0, 760.0));
    let view = demo::fixture(ScreenId::Layout);
    harness.settle(&view);
    let peer = harness.rect("Other machine").center();
    harness.frame(&view, vec![Event::PointerMoved(peer)]);
    harness.frame(&view, vec![pointer(peer, true)]);
    let moved = peer + egui::vec2(70.0, 24.0);
    harness.frame(&view, vec![Event::PointerMoved(moved)]);
    harness.shell.cancel_layout_drag();
    harness.frame(&view, Vec::new());
    let cancelled = harness.rect("Other machine");
    harness.frame(
        &view,
        vec![Event::PointerMoved(moved + egui::vec2(100.0, 60.0))],
    );
    assert_eq!(harness.rect("Other machine"), cancelled);
    harness.frame(&view, vec![pointer(moved, false)]);
    let actions = harness.click(&view, "Apply");
    assert!(
        matches!(&actions[..], [WizardAction { intent: WizardIntent::Layout(crosspane_ui_kit::layout::LayoutAction::Apply(placements)), .. }] if placements.iter().any(|placement| placement.node == "peer" && placement.origin_mm != [300.0, 0.0]))
    );
}

#[test]
fn layout_press_release_and_drag_spanning_revisions_are_retired() {
    let mut harness = Harness::new(egui::vec2(1100.0, 760.0));
    let mut view = demo::fixture(ScreenId::Layout);
    harness.settle(&view);
    let peer = harness.rect("Other machine").center();
    harness.frame(&view, vec![Event::PointerMoved(peer)]);
    harness.frame(&view, vec![pointer(peer, true)]);
    let moved = peer + egui::vec2(70.0, 24.0);
    harness.frame(&view, vec![Event::PointerMoved(moved)]);
    harness.frame(&view, vec![pointer(moved, false)]);
    harness.settle(&view);
    let apply = harness.rect("Apply").center();
    harness.frame(&view, vec![Event::PointerMoved(apply)]);
    harness.frame(&view, vec![pointer(apply, true)]);
    view.revision += 1;
    assert!(harness.frame(&view, vec![pointer(apply, false)]).is_empty());
    harness.settle(&view);
    let peer = harness.rect("Other machine").center();
    harness.frame(&view, vec![Event::PointerMoved(peer)]);
    harness.frame(&view, vec![pointer(peer, true)]);
    let moved = peer + egui::vec2(30.0, 12.0);
    harness.frame(&view, vec![Event::PointerMoved(moved)]);
    view.revision += 1;
    assert!(
        harness
            .frame(
                &view,
                vec![Event::PointerMoved(moved + egui::vec2(60.0, 30.0))]
            )
            .is_empty()
    );
    let retired = harness.rect("Other machine");
    harness.frame(
        &view,
        vec![Event::PointerMoved(moved + egui::vec2(120.0, 60.0))],
    );
    assert_eq!(harness.rect("Other machine"), retired);
    harness.frame(&view, vec![pointer(moved, false)]);
    assert!(matches!(
        &harness.click(&view, "Apply")[..],
        [WizardAction {
            revision: 3,
            intent: WizardIntent::Layout(_),
            ..
        }]
    ));
}

#[test]
fn overflowing_consent_scrolls_into_view_and_cannot_activate_while_hidden() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::RepairRemove);
    view.rows = (0..24)
        .map(|id| RowView {
            id,
            label: format!("Removal detail {id}"),
            detail: "Review this owned change before removal. ".repeat(8),
            state: RowState::Unchecked,
            human_confirmed: false,
        })
        .collect();
    harness.settle(&view);
    let label = "Delete this machine's Crosspane identity and trust";
    assert!(!harness.texts().iter().any(|(text, _)| text == label));
    harness.frame(&view, vec![key(Key::Tab, false)]);
    // Tab gives the still-hidden checkbox focus and Enter arrives in that same pass.
    assert!(
        harness
            .frame(&view, vec![key(Key::Tab, false), key(Key::Enter, false)])
            .is_empty()
    );
    let consent_focus = harness.ctx.memory(|memory| memory.focused()).unwrap();
    harness.frame(&view, Vec::new());
    assert!(harness.rect(label).is_positive());
    assert_eq!(
        harness.ctx.memory(|memory| memory.focused()),
        Some(consent_focus)
    );
    assert!(harness.focused_label(label));
    assert_eq!(
        harness.frame(&view, vec![key(Key::Enter, false)]),
        vec![WizardAction {
            revision: 1,
            intent: WizardIntent::SetToggle {
                field: 20,
                checked: true
            }
        }]
    );
}

#[test]
fn repeated_vertical_arrows_scroll_without_moving_focus() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::Practice);
    view.illustration.practice = None;
    view.rows = (0..40)
        .map(|id| RowView {
            id,
            label: format!("Practice detail {id}"),
            detail: "Waiting for evidence. ".repeat(12),
            state: RowState::Waiting,
            human_confirmed: false,
        })
        .collect();
    harness.settle(&view);
    harness.frame(&view, vec![key(Key::Tab, false)]);
    let scroll_focus = harness.ctx.memory(|memory| memory.focused()).unwrap();
    let before = harness.texts();
    for _ in 0..6 {
        harness.frame(&view, vec![key(Key::ArrowDown, false)]);
        assert_eq!(
            harness.ctx.memory(|memory| memory.focused()),
            Some(scroll_focus)
        );
    }
    let after = harness.texts();
    assert_ne!(before, after);
    for _ in 0..3 {
        harness.frame(&view, vec![key(Key::ArrowUp, false)]);
        assert_eq!(
            harness.ctx.memory(|memory| memory.focused()),
            Some(scroll_focus)
        );
    }
    assert_ne!(harness.texts(), after);
}

#[test]
fn clipped_layout_toolbar_cannot_apply_or_revert_before_focus_reveal() {
    for target in ["Apply", "Revert"] {
        let mut harness = Harness::new(egui::vec2(800.0, 3000.0));
        let mut view = demo::fixture(ScreenId::Layout);
        view.rows = (0..8)
            .map(|id| RowView {
                id,
                label: format!("Layout detail {id}"),
                detail: "Review this layout before committing changes. ".repeat(8),
                state: RowState::Unchecked,
                human_confirmed: false,
            })
            .collect();
        harness.settle(&view);
        let peer = harness.rect("Other machine").center();
        harness.frame(&view, vec![Event::PointerMoved(peer)]);
        harness.frame(&view, vec![pointer(peer, true)]);
        let moved = peer + egui::vec2(70.0, 24.0);
        harness.frame(&view, vec![Event::PointerMoved(moved)]);
        harness.frame(&view, vec![pointer(moved, false)]);
        harness.settle(&view);
        if let Some(focused) = harness.ctx.memory(|memory| memory.focused()) {
            harness
                .ctx
                .memory_mut(|memory| memory.surrender_focus(focused));
        }
        if target == "Revert" {
            harness.focus_label(&view, "Apply");
        } else {
            harness.frame(&view, vec![key(Key::Tab, false)]);
        }
        // A viewport resize clips the toolbar without changing the supplied plan.
        // Tab and Enter reach its next control in the same clipped paint.
        harness.size = egui::vec2(800.0, 600.0);
        assert!(
            harness
                .frame(&view, vec![key(Key::Tab, false), key(Key::Enter, false)])
                .is_empty()
        );
        harness.frame(&view, Vec::new());
        assert!(harness.focused_label(target));
        let actions = harness.frame(&view, vec![key(Key::Enter, false)]);
        assert!(
            matches!(&actions[..], [WizardAction { intent: WizardIntent::Layout(action), .. }]
            if matches!((target, action), ("Apply", crosspane_ui_kit::layout::LayoutAction::Apply(_)) | ("Revert", crosspane_ui_kit::layout::LayoutAction::Revert)))
        );
    }
}

#[test]
fn progress_uses_supplied_groups_and_the_actual_viewport_breakpoint() {
    for size in [egui::vec2(1100.0, 760.0), egui::vec2(800.0, 600.0)] {
        let mut harness = Harness::new(size);
        let mut view = demo::fixture(ScreenId::Welcome);
        view.progress = ProgressView {
            current: Some(ProgressGroup::Arrange),
            completed: vec![ProgressGroup::Connect],
        };
        harness.settle(&view);
        let texts = harness.texts();
        if size.x == 1100.0 {
            for label in [
                "Install",
                "Permissions / Network",
                "Connect",
                "Arrange",
                "Practice",
                "Ready",
            ] {
                assert!(texts.iter().any(|(text, _)| text == label));
            }
            assert_eq!(
                texts.iter().filter(|(text, _)| text == "Completed").count(),
                1
            );
            assert_eq!(
                texts.iter().filter(|(text, _)| text == "Current").count(),
                1
            );
            assert!(!texts.iter().any(|(text, _)| text == "Current: Arrange"));
        } else {
            assert!(texts.iter().any(|(text, _)| text == "Current: Arrange"));
            assert!(
                texts
                    .iter()
                    .any(|(text, _)| text == "1 of 6 groups completed")
            );
            assert!(
                !texts
                    .iter()
                    .any(|(text, _)| text == "Permissions / Network")
            );
        }
    }
}

#[test]
fn compact_fixtures_show_the_whole_first_state_card_without_scrolling() {
    for (screen, _) in demo::SCREENS {
        let mut harness = Harness::review(egui::vec2(800.0, 600.0));
        let view = demo::fixture(screen);
        let before = view.clone();
        harness.settle(&view);
        if let Some(first) = view.rows.first() {
            let (card, body_clip) = harness.card(&first.label);
            assert!(
                body_clip.expand(1.0).contains_rect(card),
                "First {screen:?} card {card:?} escaped body {body_clip:?}"
            );
            for label in [&first.label, &first.detail] {
                // Match the text inside this card, not Audio's duplicate outside privacy copy.
                assert!(
                    harness.shapes().iter().any(|(shape, clip)| match shape {
                        egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                            let raw = text.galley.rect.translate(text.pos.to_vec2());
                            card.contains_rect(raw) && clip.expand(1.0).contains_rect(raw)
                        }
                        _ => false,
                    }),
                    "First {screen:?} card has a clipped {label:?}"
                );
            }
        }
        assert_eq!(view, before);
        for button in &view.buttons {
            assert!(
                harness
                    .raw_text(&button.label)
                    .1
                    .contains_rect(harness.raw_text(&button.label).0)
            );
        }
    }
}

fn solid_scrollbar(harness: &Harness) -> Option<(Rect, Rect)> {
    let shapes = harness.shapes();
    shapes.iter().find_map(|(shape, clip)| {
        let egui::epaint::Shape::Rect(track) = shape else {
            return None;
        };
        if !(track.rect.width() >= 5.0
            && track.rect.width() <= 8.0
            && track.rect.height() > 60.0
            && track.fill == theme::MIDNIGHT
            && clip.contains_rect(track.rect))
        {
            return None;
        }
        shapes.iter().find_map(|(shape, handle_clip)| match shape {
            egui::epaint::Shape::Rect(handle)
                if handle.fill == theme::ICE
                    && track.rect.contains_rect(handle.rect)
                    && handle.rect.height() < track.rect.height()
                    && handle.rect.width() >= 5.0
                    && handle_clip.contains_rect(handle.rect) =>
            {
                Some((track.rect, handle.rect))
            }
            _ => None,
        })
    })
}

#[test]
fn overflow_has_a_dormant_solid_track_and_handle_and_keeps_fixed_actions_visible() {
    for size in [egui::vec2(800.0, 600.0), egui::vec2(1100.0, 760.0)] {
        let mut harness = Harness::review(size);
        let mut view = demo::fixture(ScreenId::Practice);
        view.rows = (0..40)
            .map(|id| RowView {
                id,
                label: format!("Overflow state {id}"),
                detail: "Waiting for actual evidence. ".repeat(10),
                state: RowState::Waiting,
                human_confirmed: false,
            })
            .collect();
        harness.settle(&view);
        let (track, handle) =
            solid_scrollbar(&harness).expect("Overflow must show a solid dormant scrollbar");
        let (card, body_clip) = harness.card(&view.rows[0].label);
        assert!(
            track.left() >= card.right() + 2.0,
            "Scrollbar floats over the content: track {track:?}, card {card:?}, clip {body_clip:?}"
        );
        assert!(handle.height() < track.height());
        let content_top = if size.y == 600.0 {
            card.top()
        } else {
            harness.raw_text(&view.message).0.top()
        };
        assert!(
            (content_top - body_clip.top()).abs() <= 4.0,
            "Overflow must stay top-aligned: content {content_top}, body {body_clip:?}"
        );
        for label in ["Release control", "Stop practice"] {
            let (raw, clip, _) = harness.raw_text(label);
            assert!(clip.contains_rect(raw) && raw.bottom() <= size.y);
        }
        let mut short = Harness::review(size);
        let mut short_view = demo::fixture(ScreenId::Welcome);
        // The full welcome paragraph can overflow with a wider system font.
        // Keep this no-overflow control intentionally short for every font.
        short_view.message = "Short body".into();
        short.settle(&short_view);
        assert!(solid_scrollbar(&short).is_none());
    }
}

fn body_content_bounds(harness: &Harness, clip: Rect) -> Rect {
    harness
        .shapes()
        .into_iter()
        .filter_map(|(shape, shape_clip)| {
            if !shape_clip.is_positive() || !clip.expand(1.0).contains_rect(shape_clip) {
                return None;
            }
            match shape {
                egui::epaint::Shape::Noop => None,
                egui::epaint::Shape::Rect(rect)
                    if rect.blur_width > 0.0
                        || (rect.fill.a() == 0 && rect.stroke.color.a() == 0) =>
                {
                    None
                }
                egui::epaint::Shape::Text(text) => {
                    Some(text.galley.rect.translate(text.pos.to_vec2()))
                }
                _ => Some(shape.visual_bounding_rect()),
            }
        })
        .filter(|rect| rect.is_positive())
        .fold(Rect::NOTHING, Rect::union)
}

#[test]
fn short_whole_bodies_with_choices_and_controls_are_centered_at_review_size() {
    for (screen, include_row) in [
        (ScreenId::MatchNumbers, false),
        (ScreenId::HidingChoice, false),
        (ScreenId::Grants, false),
        (ScreenId::RepairRemove, false),
        (ScreenId::RepairRemove, true),
    ] {
        let mut harness = Harness::review(egui::vec2(1100.0, 760.0));
        let mut view = demo::fixture(screen);
        if include_row {
            view.rows = vec![RowView {
                id: 30,
                label: "Owned user changes".into(),
                detail: "Review the supplied removal plan".into(),
                state: RowState::NeedsAction,
                human_confirmed: false,
            }];
            view.fields.truncate(1);
        }
        harness.settle(&view);
        let body_clip = harness.raw_text(&view.message).1;
        let bounds = body_content_bounds(&harness, body_clip);
        assert!(
            body_clip.expand(1.0).contains_rect(bounds),
            "Short {screen:?} body overflows: {bounds:?} in {body_clip:?}"
        );
        let top = bounds.top() - body_clip.top();
        let bottom = body_clip.bottom() - bounds.bottom();
        assert!(
            (top - bottom).abs() <= 20.0,
            "Short {screen:?} whole body is off-center: top {top}, bottom {bottom}"
        );
    }
}

#[test]
fn component_and_direction_diagrams_are_bounded_and_explanatory_at_both_sizes() {
    for size in [egui::vec2(800.0, 600.0), egui::vec2(1100.0, 760.0)] {
        for (screen, caption, labels) in [
            (
                ScreenId::InstallPlan,
                "Preview: user-owned agent and startup; optional audio package affects all users",
                &["User agent", "User startup", "Audio driver"][..],
            ),
            (
                ScreenId::Grants,
                "Control and windows · each direction is a separate opt-in",
                &["This machine", "Other machine"][..],
            ),
        ] {
            let mut harness = Harness::review(size);
            let view = demo::fixture(screen);
            let before = view.clone();
            harness.settle(&view);
            // State rows keep priority. Reveal the diagram through keyboard scrolling
            // when needed, stopping before scrolling past art ahead of grant controls.
            let diagram_visible = |harness: &Harness| {
                let (raw, clip, _) = harness.raw_text(caption);
                clip.height() >= 95.0
                    && clip.contains_rect(raw)
                    && labels.iter().all(|label| {
                        let (raw, clip, _) = harness.raw_text(label);
                        clip.contains_rect(raw)
                    })
            };
            if !diagram_visible(&harness) {
                harness.frame(&view, vec![key(Key::Tab, false)]);
                for _ in 0..32 {
                    assert!(
                        harness
                            .frame(&view, vec![key(Key::ArrowDown, false)])
                            .is_empty()
                    );
                    harness.frame(&view, Vec::new());
                    if diagram_visible(&harness) {
                        break;
                    }
                }
            }
            let (raw, clip, _) = harness.raw_text(caption);
            assert!(
                clip.contains_rect(raw) && clip.height() <= 97.0,
                "{screen:?} diagram caption {raw:?} in {clip:?}"
            );
            for label in labels {
                let (raw, label_clip, _) = harness.raw_text(label);
                assert!(
                    label_clip.contains_rect(raw),
                    "Clipped diagram label {label}"
                );
            }
            let colors = harness
                .shapes()
                .into_iter()
                .filter_map(|(shape, shape_clip)| match shape {
                    egui::epaint::Shape::Rect(rect)
                        if clip.contains_rect(shape_clip) && rect.rect.width() > 60.0 =>
                    {
                        Some(rect.stroke.color)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(colors.contains(&theme::FROST));
            assert!(colors.contains(&if screen == ScreenId::Grants {
                theme::PEER_ICE
            } else {
                theme::GLACIER
            }));
            if screen == ScreenId::Grants {
                let arrows = harness
                    .shapes()
                    .into_iter()
                    .filter_map(|(shape, shape_clip)| match shape {
                        egui::epaint::Shape::LineSegment { points, stroke }
                            if clip.contains_rect(shape_clip)
                                && stroke.color == theme::QUIET
                                && (points[0].y - points[1].y).abs() < 0.5
                                && (points[0].x - points[1].x).abs() > 15.0 =>
                        {
                            Some(points[1].x - points[0].x)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert!(arrows.iter().any(|direction| *direction > 0.0));
                assert!(arrows.iter().any(|direction| *direction < 0.0));
            }
            assert_eq!(view, before);
        }
    }
}

#[test]
fn unchecked_choice_indicators_have_visible_kit_outlines_without_hover() {
    for screen in [
        ScreenId::HidingChoice,
        ScreenId::Grants,
        ScreenId::RepairRemove,
    ] {
        let mut harness = Harness::review(egui::vec2(1100.0, 760.0));
        let view = demo::fixture(screen);
        let before = view.clone();
        harness.settle(&view);
        let labels = if screen == ScreenId::HidingChoice {
            vec![demo::HIDE_LABEL, demo::MIRROR_LABEL]
        } else {
            view.fields
                .iter()
                .filter_map(|field| match field {
                    FieldView::Toggle {
                        checked: false,
                        label,
                        ..
                    } => Some(label.as_str()),
                    _ => None,
                })
                .collect()
        };
        for label in labels {
            let label_rect = harness.raw_text(label).0;
            let indicator_region = Rect::from_min_max(
                label_rect.min - egui::vec2(30.0, 4.0),
                egui::pos2(label_rect.left(), label_rect.bottom() + 4.0),
            );
            assert!(
                harness.shapes().into_iter().any(|(shape, clip)| {
                    let (bounds, stroke) = match shape {
                        egui::epaint::Shape::Circle(circle) => (
                            Rect::from_center_size(
                                circle.center,
                                egui::vec2(2.0 * circle.radius, 2.0 * circle.radius),
                            ),
                            circle.stroke,
                        ),
                        egui::epaint::Shape::Rect(rect) => (rect.rect, rect.stroke),
                        _ => return false,
                    };
                    indicator_region.contains(bounds.center())
                        && clip.contains_rect(bounds)
                        && stroke.width >= 1.0
                        && stroke.color == theme::GLACIER
                        && stroke.color.a() == 255
                }),
                "No visible unselected indicator for {label}"
            );
        }
        assert_eq!(view, before);
    }
}

#[test]
fn network_caption_and_traffic_dot_follow_only_the_supplied_traffic_fact() {
    assert!(
        demo::fixture(ScreenId::Network)
            .illustration
            .traffic_observed
    );
    for observed in [false, true] {
        let mut harness = Harness::review(egui::vec2(800.0, 600.0));
        let mut view = demo::fixture(ScreenId::Network);
        view.illustration.traffic_observed = observed;
        let before = view.clone();
        harness.settle(&view);
        harness.frame(&view, vec![key(Key::Tab, false)]);
        for _ in 0..8 {
            harness.frame(&view, vec![key(Key::PageDown, false)]);
        }
        harness.settle(&view);
        let label = if observed {
            "Traffic observed"
        } else {
            "Waiting for observed traffic"
        };
        let color = if observed { theme::FROST } else { theme::QUIET };
        let (raw, clip, text) = harness.raw_text(label);
        assert!(clip.contains_rect(raw));
        assert_eq!(text.fallback_color, color);
        assert!(
            text.galley
                .rows
                .iter()
                .flat_map(|row| &row.visuals.mesh.vertices)
                .all(|vertex| vertex.color == color)
        );
        let dot = harness.shapes().into_iter().any(|(shape, shape_clip)| matches!(shape,
            egui::epaint::Shape::Circle(circle) if clip.contains_rect(shape_clip) && circle.fill == theme::GLACIER));
        assert_eq!(dot, observed);
        assert_eq!(view, before);
        assert_eq!(view.rows[0].state, RowState::Waiting);
    }
}

#[test]
fn completed_current_ready_rail_group_shows_one_status_label() {
    let mut harness = Harness::review(egui::vec2(1100.0, 760.0));
    let view = demo::fixture(ScreenId::Summary);
    harness.settle(&view);
    let ready = harness.raw_text("Ready").0;
    let labels = harness
        .shapes()
        .into_iter()
        .filter_map(|(shape, _)| match shape {
            egui::epaint::Shape::Text(text)
                if matches!(text.galley.text(), "Current" | "Completed") =>
            {
                let rect = text.galley.rect.translate(text.pos.to_vec2());
                (rect.top() > ready.bottom() && rect.top() < ready.bottom() + 55.0)
                    .then_some(text.galley.text())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(labels, vec!["Completed"]);
}

fn check(index: usize, label: &str, state: RowState, detail: &str) -> RowView {
    RowView {
        id: check_row_id(index),
        label: label.into(),
        detail: detail.into(),
        state,
        human_confirmed: false,
    }
}

#[test]
fn the_support_checklist_is_drawn_inside_its_card_with_its_own_wording_in_every_motion() {
    for motion in [
        MotionPreference::Reduced,
        MotionPreference::Full,
        MotionPreference::Auto,
    ] {
        // The review size; the compact size scrolls the body, which other tests cover.
        for size in [egui::vec2(1100.0, 760.0)] {
            let mut harness = Harness::review(size);
            let mut view = demo::fixture(ScreenId::Compatibility);
            view.motion = motion;
            view.rows = vec![
                RowView {
                    id: 10,
                    label: "This computer can run Crosspane".into(),
                    detail: "Some facts couldn't be confirmed yet. Last checked 3 s ago.".into(),
                    state: RowState::Waiting,
                    human_confirmed: false,
                },
                check(0, "Operating system", RowState::Verified, "Arch-based"),
                check(1, "Processor", RowState::Verified, ""),
                check(2, "Hyprland version", RowState::Failed, "too old"),
                check(
                    3,
                    "This session is the signed-in one",
                    RowState::Waiting,
                    "couldn't read the session environment",
                ),
                check(4, "Required libraries", RowState::Working, ""),
            ];
            harness.settle(&view);
            let (card, _) = harness.card("This computer can run Crosspane");
            for line in [
                "Operating system  Passed (Arch-based)",
                "Processor  Passed",
                "Hyprland version  Failed: too old",
                "This session is the signed-in one  Couldn't confirm: couldn't read the session \
                 environment",
                "Required libraries  Checking…",
            ] {
                let (rect, _, _) = harness.raw_text(line);
                assert!(
                    card.contains_rect(rect),
                    "{motion:?} {size:?}: {line:?} {rect:?} outside the card {card:?}"
                );
            }
            // Checks are lines in the card, not cards of their own: the card count is the same
            // as with the card alone.
            let glass = |harness: &Harness| {
                harness
                    .shapes()
                    .into_iter()
                    .filter(|(shape, _)| {
                        matches!(shape, egui::epaint::Shape::Rect(rect)
                            if rect.fill == theme::alpha(theme::MIDNIGHT, 205)
                                && rect.stroke.color == theme::alpha(theme::GLACIER, 42))
                    })
                    .count()
            };
            let with_checks = glass(&harness);
            let mut alone = Harness::review(size);
            let mut card_only = view.clone();
            card_only.rows.truncate(1);
            alone.settle(&card_only);
            assert_eq!(with_checks, glass(&alone), "{motion:?} {size:?}");
            // Nothing in the checklist animates: a settled frame asks for no repaint.
            if motion == MotionPreference::Reduced {
                let output = harness.output.as_ref().unwrap();
                assert!(
                    output
                        .viewport_output
                        .values()
                        .all(|v| v.repaint_delay > std::time::Duration::from_millis(100)),
                    "{size:?}: a reduced, settled checklist keeps repainting"
                );
            }
        }
    }
}

#[test]
fn checklist_rows_without_a_card_get_one_and_ids_stay_in_their_range() {
    assert!(CHECK_ROW_IDS.contains(&check_row_id(0)));
    assert!(CHECK_ROW_IDS.contains(&check_row_id(10_000)));
    assert!(!CHECK_ROW_IDS.contains(&59) && !CHECK_ROW_IDS.contains(&90));
    let mut harness = Harness::review(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::Compatibility);
    view.rows = vec![check(0, "Operating system", RowState::Unchecked, "")];
    harness.settle(&view);
    let (card, _) = harness.card("Operating system  Not checked yet");
    assert!(card.is_positive());
}
