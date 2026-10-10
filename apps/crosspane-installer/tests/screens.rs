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

    /// Long enough for every finite motion of a page (transition, rows, height) to finish.
    fn settle(&mut self, view: &WizardView) {
        for _ in 0..16 {
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
                    if rect.fill == theme::card_fill()
                        && rect.stroke.color == theme::card_stroke()
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

/// The buttons drawn in the fixed footer: Back, and every outlined or filled action. Links and
/// choices belong to the scrolling content.
fn footer(view: &WizardView) -> Vec<&ButtonView> {
    view.buttons
        .iter()
        .filter(|button| {
            button.role == ButtonRole::Back
                || matches!(
                    button.kind,
                    ButtonKind::Primary | ButtonKind::Secondary | ButtonKind::Destructive
                )
        })
        .collect()
}

#[test]
fn all_fourteen_screens_render_with_demo_label_and_supplied_state_at_both_sizes() {
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
            for button in footer(&view) {
                assert!(
                    harness.rect(&button.label).max.y <= size.y,
                    "Footer escaped viewport on {screen:?}"
                );
            }
            // A screen asks one question at a time: never more than one filled button.
            assert!(
                view.buttons
                    .iter()
                    .filter(|button| button.kind == ButtonKind::Primary)
                    .count()
                    <= 1,
                "{screen:?} has several primary buttons"
            );
        }
        for (name, _) in demo::VARIANTS {
            let view = demo::fixture_named(name).unwrap();
            harness.settle(&view);
            assert!(harness.texts().iter().any(|(text, _)| text == &view.title));
            for button in footer(&view) {
                assert!(
                    harness.rect(&button.label).max.y <= size.y,
                    "Footer escaped viewport on {name}"
                );
            }
        }
    }
}

#[test]
fn rows_read_as_marks_and_plain_words_without_status_vocabulary() {
    let mut harness = Harness::review(egui::vec2(1100.0, 760.0));
    for (screen, _) in demo::SCREENS {
        let view = demo::fixture(screen);
        harness.settle(&view);
        for word in [
            "Needs your action",
            "Verified",
            "Working",
            "Waiting",
            "Unsupported",
            "Not checked",
        ] {
            assert!(
                !harness.texts().iter().any(|(text, _)| text == word),
                "{screen:?} shows the status word {word:?}"
            );
        }
    }
    // A failure reads as its own sentence, next to the fix.
    let view = demo::fixture_named("install-failed").unwrap();
    harness.settle(&view);
    let failed = view
        .rows
        .iter()
        .find(|row| row.state == RowState::Failed)
        .unwrap();
    assert!(
        harness
            .texts()
            .iter()
            .any(|(text, _)| text == &failed.detail)
    );
    assert_eq!(
        footer(&view)
            .iter()
            .find(|button| button.kind == ButtonKind::Primary)
            .map(|button| button.label.as_str()),
        Some("Try again")
    );
}

#[test]
fn footer_buttons_never_wrap_and_move_to_a_new_row_when_narrow() {
    let mut harness = Harness::review(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::RepairRemove);
    for (id, label) in [
        (40, "Look for computers nearby"),
        (41, "Reconnect a computer paired before"),
        (42, "Let the other computer join"),
    ] {
        view.buttons.push(ButtonView {
            id,
            role: ButtonRole::Ordinary,
            label: label.into(),
            enabled: true,
            kind: ButtonKind::Secondary,
        });
    }
    harness.settle(&view);
    let line_height = harness.raw_text("Back").0.height();
    let mut tops = Vec::new();
    for button in footer(&view) {
        let (raw, clip, text) = harness.raw_text(&button.label);
        assert_eq!(
            text.galley.rows.len(),
            1,
            "{:?} wrapped inside its button",
            button.label
        );
        assert!(raw.height() < line_height * 1.6);
        assert!(
            clip.contains_rect(raw) && raw.right() <= 800.0,
            "{:?} overflows",
            button.label
        );
        tops.push(raw.top());
    }
    tops.sort_by(f32::total_cmp);
    tops.dedup_by(|a, b| (*a - *b).abs() < 2.0);
    assert!(
        tops.len() >= 2,
        "the buttons that don't fit move to another row"
    );
}

#[test]
fn enabled_disabled_buttons_emit_only_rendered_revision() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::Welcome);
    view.revision = 42;
    view.buttons[0].enabled = false;
    harness.settle(&view);
    assert!(harness.click(&view, "Start setup").is_empty());
    view.buttons[0].enabled = true;
    view.revision = 43;
    harness.settle(&view);
    assert_eq!(
        harness.click(&view, "Start setup"),
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
    if cfg!(windows) {
        assert!(
            !harness
                .texts()
                .iter()
                .any(|(text, _)| text == demo::REMOVE_AUDIO_LABEL)
        );
    } else {
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
    }
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
    // The address field appears only once the person chose to type one.
    assert!(
        !demo::fixture(ScreenId::Connect)
            .fields
            .iter()
            .any(|field| matches!(field, FieldView::PeerAddress { .. }))
    );
    let mut view = demo::fixture_named("connect-address").unwrap();
    harness.settle(&view);
    harness.click(&view, "For example 192.168.1.20:47811");
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
    harness.focus_label(&view, "Start setup");
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
    harness.focus_label(&view, "Start setup");
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
        assert!(!harness.focused_label("Apply and restart Crosspane"));
        assert!(!harness.focused_label("Back"));
    }
}

#[test]
fn keyboard_scrolls_long_content_while_release_and_stop_remain_visible() {
    let mut harness = Harness::new(egui::vec2(800.0, 600.0));
    let mut view = demo::fixture(ScreenId::Grants);
    view.rows = (0..40)
        .map(|id| RowView {
            id,
            label: format!("Permission result {id}"),
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
    assert!(harness.rect("Allow all and continue").max.y <= 600.0);
    assert!(harness.rect("Back").max.y <= 600.0);
    assert_eq!(
        harness.click(&view, "Allow all and continue"),
        vec![WizardAction {
            revision: 1,
            intent: WizardIntent::Button(crosspane_installer::live::ids::GRANTS_ALL)
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
    let mut view = demo::fixture(ScreenId::Grants);
    view.rows = (0..40)
        .map(|id| RowView {
            id,
            label: format!("Permission detail {id}"),
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
        let labels: &[&str] = if cfg!(windows) {
            &["Install", "Connect", "Arrange", "Ready"]
        } else {
            &["Install", "Permissions", "Connect", "Arrange", "Ready"]
        };
        // Progress reads by marks, not by "Current"/"Completed" words.
        for word in ["Current", "Completed", "Current: Arrange"] {
            assert!(!texts.iter().any(|(text, _)| text == word));
        }
        // The narrow strip: one segment per platform group, completed Frost, current Glacier.
        let segments: Vec<egui::Color32> = harness
            .shapes()
            .into_iter()
            .filter_map(|(shape, _)| match shape {
                egui::epaint::Shape::Rect(rect)
                    if (rect.rect.height() - 4.0).abs() < 0.5 && rect.rect.width() > 40.0 =>
                {
                    Some(rect.fill)
                }
                _ => None,
            })
            .collect();
        if size.x == 1100.0 {
            for &label in labels {
                assert!(texts.iter().any(|(text, _)| text == label), "{label}");
            }
            assert!(segments.is_empty(), "the rail replaces the strip");
        } else {
            for &label in labels {
                assert!(
                    !texts.iter().any(|(text, _)| text == label),
                    "the narrow layout has no rail: {label}"
                );
            }
            assert_eq!(segments.len(), labels.len());
            assert_eq!(segments.iter().filter(|c| **c == theme::FROST).count(), 1);
            assert_eq!(segments.iter().filter(|c| **c == theme::GLACIER).count(), 1);
        }
    }
}

#[test]
fn compact_fixtures_show_the_first_step_and_the_footer_without_scrolling() {
    for (screen, _) in demo::SCREENS {
        let mut harness = Harness::review(egui::vec2(800.0, 600.0));
        let view = demo::fixture(screen);
        let before = view.clone();
        harness.settle(&view);
        if let Some(first) = view.rows.first() {
            let (card, _) = harness.card(&first.label);
            let mut labels = vec![&first.label];
            if !matches!(first.state, RowState::Verified | RowState::Unchecked) {
                labels.push(&first.detail);
            }
            for label in labels {
                // Match the text inside the card, not Audio's duplicate outside privacy copy.
                assert!(
                    harness.shapes().iter().any(|(shape, clip)| match shape {
                        egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                            let raw = text.galley.rect.translate(text.pos.to_vec2());
                            card.contains_rect(raw) && clip.expand(1.0).contains_rect(raw)
                        }
                        _ => false,
                    }),
                    "First {screen:?} step has a clipped {label:?}"
                );
            }
        }
        assert_eq!(view, before);
        for button in footer(&view) {
            let (raw, clip, _) = harness.raw_text(&button.label);
            assert!(clip.contains_rect(raw), "{screen:?}: {}", button.label);
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
        let mut view = demo::fixture(ScreenId::Grants);
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
        let (message, body_clip, _) = harness.raw_text(&view.message);
        // No text of the scrolling body sits under the scrollbar.
        for (text, rect) in harness.texts() {
            if body_clip.contains_rect(rect) && rect.top() < track.bottom() {
                assert!(
                    rect.right() <= track.left() + 0.5,
                    "Scrollbar floats over {text:?}: track {track:?}, text {rect:?}"
                );
            }
        }
        assert!(handle.height() < track.height());
        assert!(
            (message.top() - body_clip.top()).abs() <= 4.0,
            "Overflow must stay top-aligned: content {message:?}, body {body_clip:?}"
        );
        for label in ["Allow all and continue", "Back"] {
            let (raw, clip, _) = harness.raw_text(label);
            assert!(clip.contains_rect(raw) && raw.bottom() <= size.y);
        }
        let mut short = Harness::review(size);
        let mut short_view = demo::fixture(ScreenId::Connect);
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
                // What is painted is clipped: a soft glow may reach past its clip.
                _ => Some(shape.visual_bounding_rect().intersect(shape_clip)),
            }
        })
        .filter(|rect| rect.is_positive())
        .fold(Rect::NOTHING, Rect::union)
}

#[test]
fn short_bodies_with_choices_and_controls_leave_no_dead_space_at_review_size() {
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
        // No dead space: the content starts under the title, and the footer follows the
        // content directly (the card is as tall as what it holds).
        let title = harness.raw_text(&view.title).0;
        assert!(
            (bounds.top() - body_clip.top()).abs() <= 4.0,
            "Short {screen:?} body is padded at the top"
        );
        assert!(
            bounds.top() - title.bottom() <= 70.0,
            "Short {screen:?} content starts far below its title: {bounds:?} after {title:?}"
        );
        if let Some(button) = footer(&view).first() {
            let footer_top = harness.raw_text(&button.label).0.top();
            assert!(
                footer_top - bounds.bottom() <= 64.0,
                "Short {screen:?} footer floats below its content: {footer_top} after {bounds:?}"
            );
        }
    }
}

#[test]
fn direction_diagram_is_bounded_and_decoration_gives_way_in_short_windows() {
    // At the review size the grants diagram shows both machines and both directions.
    let mut harness = Harness::review(egui::vec2(1100.0, 760.0));
    let view = demo::fixture(ScreenId::Grants);
    let before = view.clone();
    harness.settle(&view);
    let (_, clip, _) = harness.raw_text("This machine");
    for label in ["This machine", "Other machine"] {
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
    assert!(colors.contains(&theme::PEER_ICE));
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
    // In a short window the decoration gives way, so the controls come first.
    let mut short = Harness::review(egui::vec2(800.0, 600.0));
    short.settle(&view);
    assert!(
        !short.texts().iter().any(|(text, _)| text == "This machine"),
        "a decorative diagram crowds the controls in a short window"
    );
    assert_eq!(view, before);
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
        // The diagram belongs to the wait for traffic, not to the firewall question.
        view.rows[0].state = RowState::Waiting;
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
        assert_ne!(view.rows[0].state, RowState::Verified);
    }
}

#[test]
fn completed_current_ready_rail_group_shows_one_status_mark() {
    let mut harness = Harness::review(egui::vec2(1100.0, 760.0));
    let view = demo::fixture(ScreenId::Summary);
    harness.settle(&view);
    let ready = harness.raw_text("Ready").0;
    // The marks just left of the label: one filled Done disc, not a Current ring and dot too.
    let region = Rect::from_min_max(
        egui::pos2(ready.left() - 40.0, ready.top() - 6.0),
        egui::pos2(ready.left(), ready.bottom() + 6.0),
    );
    let discs: Vec<f32> = harness
        .shapes()
        .into_iter()
        .filter_map(|(shape, _)| match shape {
            egui::epaint::Shape::Circle(circle)
                if region.contains(circle.center) && circle.fill == theme::FROST =>
            {
                Some(circle.radius)
            }
            _ => None,
        })
        .collect();
    assert_eq!(discs.len(), 1, "{discs:?}");
    assert!(discs[0] > 5.0, "a full disc, not the current-step dot");
    assert!(
        !harness
            .texts()
            .iter()
            .any(|(text, _)| matches!(text.as_str(), "Current" | "Completed"))
    );
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
            let open = [
                "Hyprland version  Failed: too old",
                "This session is the signed-in one  Couldn't confirm: couldn't read the session \
                 environment",
                "Required libraries  Checking…",
            ];
            let passed = ["Operating system  Passed (Arch-based)", "Processor  Passed"];
            // What needs attention is listed; what passed folds into one line.
            for line in open.iter().chain(["2 other checks passed"].iter()) {
                let (rect, _, _) = harness.raw_text(line);
                assert!(
                    card.contains_rect(rect),
                    "{motion:?} {size:?}: {line:?} {rect:?} outside the card {card:?}"
                );
            }
            for line in passed {
                assert!(
                    !harness.texts().iter().any(|(text, _)| text == line),
                    "{motion:?}: passed check {line:?} is shown before Details"
                );
            }
            // Details opens them, still inside the card.
            harness.click(&view, "Details");
            harness.settle(&view);
            let (card, _) = harness.card("This computer can run Crosspane");
            for line in passed {
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
                            if rect.fill == theme::card_fill()
                                && rect.stroke.color == theme::card_stroke())
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

/// WP-4.33: a permission row (ids from 960) draws its own actions (ids from 2600 + 10 per row)
/// under its words, never in the footer or the links under the content, and they click through.
#[test]
fn permission_rows_draw_their_own_allow_and_links_inside_the_row() {
    let mut harness = Harness::review(egui::vec2(1100.0, 760.0));
    let mut view = demo::fixture(ScreenId::Permissions);
    view.illustration.permission_row = None;
    view.rows = vec![
        RowView {
            id: 30,
            label: "Crosspane has the Mac permissions it needs".into(),
            detail: "1 of 3 allowed. Allow each one below.".into(),
            state: RowState::NeedsAction,
            human_confirmed: false,
        },
        RowView {
            id: 960,
            label: "Device Control and Data Access".into(),
            detail: "Answer the macOS request, or turn Crosspane on in System Settings.".into(),
            state: RowState::Waiting,
            human_confirmed: false,
        },
        RowView {
            id: 962,
            label: "Screen & System Audio Recording".into(),
            detail: "Allowed.".into(),
            state: RowState::Verified,
            human_confirmed: false,
        },
    ];
    view.buttons = vec![
        ButtonView {
            id: 2600,
            role: ButtonRole::Confirm,
            label: "Allow".into(),
            enabled: true,
            kind: ButtonKind::Choice,
        },
        ButtonView {
            id: 2601,
            role: ButtonRole::Confirm,
            label: "It's on".into(),
            enabled: true,
            kind: ButtonKind::Link,
        },
    ];
    harness.settle(&view);
    let label = harness.rect("Device Control and Data Access");
    let allow = harness.rect("Allow");
    let next = harness.rect("Screen & System Audio Recording");
    // Drawn inside the Accessibility row: below its words, above the next row.
    assert!(
        allow.min.y > label.min.y && allow.max.y < next.min.y,
        "{allow:?}"
    );
    let its_on = harness.rect("It's on");
    assert!(its_on.max.y < next.min.y, "{its_on:?}");
    assert!(footer(&view).is_empty(), "nothing in the footer");
    let actions = harness.click(&view, "Allow");
    assert!(
        actions
            .iter()
            .any(|a| a.intent == WizardIntent::Button(2600) && a.revision == view.revision),
        "{actions:?}"
    );
    let actions = harness.click(&view, "It's on");
    assert!(
        actions
            .iter()
            .any(|a| a.intent == WizardIntent::Button(2601)),
        "{actions:?}"
    );
}

/// The card that holds `label`, in the current frame.
fn card_of(harness: &Harness, label: &str) -> Rect {
    harness.card(label).0
}

#[test]
fn nothing_is_squished_at_the_minimum_size_or_at_retina_scale() {
    for (size, ppp) in [
        (egui::vec2(640.0, 560.0), 1.0),
        (egui::vec2(640.0, 560.0), 2.0),
        (egui::vec2(980.0, 700.0), 2.0),
    ] {
        let mut harness = Harness::new(size);
        harness.ctx.set_pixels_per_point(ppp);
        let names = demo::screens_for(demo::copy_platform())
            .iter()
            .map(|(_, name)| *name)
            .chain(demo::VARIANTS.iter().map(|(name, _)| *name));
        for name in names {
            let view = demo::fixture_named(name).unwrap();
            harness.settle(&view);
            let card = card_of(&harness, &view.title);
            let title = harness.raw_text(&view.title).0;
            assert!(
                card.contains_rect(title),
                "{name} {size:?}: title outside its card"
            );
            for button in footer(&view) {
                let (raw, clip, text) = harness.raw_text(&button.label);
                assert_eq!(
                    text.galley.rows.len(),
                    1,
                    "{name} {size:?}: {:?} wraps",
                    button.label
                );
                assert!(
                    clip.contains_rect(raw) && card.contains_rect(raw),
                    "{name} {size:?}: {:?} is cut off",
                    button.label
                );
            }
            // Nothing in the card runs past its right edge.
            for (text, rect) in harness.texts() {
                if card.contains(rect.left_center()) {
                    assert!(
                        rect.right() <= card.right() + 0.5,
                        "{name} {size:?}: {text:?} overflows the card"
                    );
                }
            }
        }
    }
}

#[test]
fn the_header_stays_where_it_is_from_screen_to_screen() {
    for size in [egui::vec2(980.0, 700.0), egui::vec2(640.0, 560.0)] {
        let mut harness = Harness::review(size);
        let mut tops = Vec::new();
        for (screen, _) in demo::SCREENS {
            let view = demo::fixture(screen);
            harness.settle(&view);
            tops.push((screen, card_of(&harness, &view.title).top()));
        }
        let first = tops[0].1;
        for (screen, top) in tops {
            assert!(
                (top - first).abs() < 0.5,
                "{screen:?} {size:?}: the card starts at {top}, not {first}"
            );
        }
    }
}

/// The left edge of `label` `ms` after `to` replaces `from`, in `motion`.
fn edge_during_change(
    from: ScreenId,
    to: ScreenId,
    motion: MotionPreference,
    label: &str,
    ms: u64,
) -> Option<f32> {
    let mut harness = Harness::review(egui::vec2(980.0, 700.0));
    let mut before = demo::fixture(from);
    before.motion = motion;
    harness.settle(&before);
    let mut after = demo::fixture(to);
    after.motion = motion;
    after.revision = 2;
    // The harness advances 50 ms a frame; the change starts on the first frame of `to`.
    for _ in 0..=(ms / 50) {
        harness.frame(&after, Vec::new());
    }
    // Unclipped: a page sliding in from the left starts partly outside the body.
    harness
        .shapes()
        .into_iter()
        .find_map(|(shape, _)| match shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                Some(text.galley.rect.translate(text.pos.to_vec2()).left())
            }
            _ => None,
        })
}

#[test]
fn pages_slide_in_their_direction_of_travel_reduced_only_fades_and_off_is_instant() {
    let label =
        "Both computers will show a number to compare.\n\nSkipping Connect also skips Arrange.";
    let rest = edge_during_change(
        ScreenId::Welcome,
        ScreenId::Connect,
        MotionPreference::Full,
        label,
        1000,
    )
    .unwrap();
    // Forward: the new page comes in from the right.
    let forward = edge_during_change(
        ScreenId::Welcome,
        ScreenId::Connect,
        MotionPreference::Full,
        label,
        150,
    )
    .unwrap();
    assert!(forward > rest + 1.0, "forward {forward} vs rest {rest}");
    // Back: from the left.
    let back = edge_during_change(
        ScreenId::Grants,
        ScreenId::Connect,
        MotionPreference::Full,
        label,
        150,
    )
    .unwrap();
    assert!(back < rest - 1.0, "back {back} vs rest {rest}");
    // Reduced: nothing moves, the page only fades.
    let reduced = edge_during_change(
        ScreenId::Welcome,
        ScreenId::Connect,
        MotionPreference::Reduced,
        label,
        50,
    )
    .unwrap();
    assert!((reduced - rest).abs() < 0.5);
    // Off: the first frame is the settled one.
    let mut harness = Harness::review(egui::vec2(980.0, 700.0));
    let mut view = demo::fixture(ScreenId::Connect);
    view.motion = MotionPreference::Off;
    harness.frame(&view, Vec::new());
    harness.frame(&view, Vec::new());
    let first = harness.texts();
    harness.settle(&view);
    assert_eq!(first, harness.texts());
}

#[test]
fn a_finished_step_animates_then_the_window_goes_idle() {
    let mut harness = Harness::review(egui::vec2(980.0, 700.0));
    let mut view = demo::fixture(ScreenId::Compatibility);
    view.motion = MotionPreference::Full;
    harness.settle(&view);
    let mut done = demo::fixture(ScreenId::InstallPlan);
    done.motion = MotionPreference::Full;
    done.revision = 2;
    harness.frame(&done, Vec::new());
    let delay = |harness: &Harness| {
        harness.output.as_ref().unwrap().viewport_output[&egui::ViewportId::ROOT].repaint_delay
    };
    // The check pops in at full frame rate…
    assert!(delay(&harness) <= std::time::Duration::from_millis(20));
    // …and once nothing is in progress or waiting, frames stop.
    let mut idle = demo::fixture(ScreenId::InstallPlan);
    idle.motion = MotionPreference::Full;
    idle.revision = 3;
    for row in &mut idle.rows {
        if !row.is_check() {
            row.state = RowState::Verified;
        }
    }
    for _ in 0..200 {
        harness.frame(&idle, Vec::new());
    }
    assert!(delay(&harness) >= std::time::Duration::from_millis(500));
}
