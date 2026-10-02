use std::time::Duration;

use crosspane_ui_kit::{art::Art, layout::LayoutWidget, theme};
use eframe::egui::{self, Color32, Id, Key, RichText, Sense, Vec2};

use crate::demo::{
    DEMO_LABEL, HIDE_LABEL, MICROPHONE_COPY, MICROPHONE_DETAIL, MIRROR_LABEL, REMOVE_AUDIO_LABEL,
};
use crate::motion::{
    HOVER_SECONDS, IllustrationMotion, TRANSITION_MS, illustration_allowed, illustration_duration,
    transition_allowed,
};
use crate::view::*;
use crate::{reduced_motion, transition_fraction};

#[derive(Default)]
pub struct WizardShell {
    layout: LayoutWidget,
    previous: Option<(ScreenId, u64)>,
    transition_start: u64,
    address_focus: Vec<(u16, Id)>,
    illustration_key: Option<(
        ScreenId,
        IllustrationView,
        SummaryView,
        Option<RowState>,
        bool,
    )>,
    illustration_motion: IllustrationMotion,
    body_measure: Option<BodyMeasure>,
}

struct BodyMeasure {
    screen: ScreenId,
    width: f32,
    viewport_height: f32,
    content_height: f32,
}

impl std::fmt::Debug for WizardShell {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WizardShell")
            .field("previous", &self.previous)
            .field("transition_start", &self.transition_start)
            .finish_non_exhaustive()
    }
}

impl WizardShell {
    pub fn follow_layout(&mut self, confirmed: &[crosspane_ui_kit::layout::DisplayRect]) {
        self.layout.follow(confirmed);
    }

    pub fn revert_layout(&mut self, confirmed: &[crosspane_ui_kit::layout::DisplayRect]) {
        self.layout.revert(confirmed);
    }

    pub fn cancel_layout_drag(&mut self) {
        self.layout.cancel_drag();
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        art: &Art,
        now_ms: u64,
    ) -> Vec<WizardAction> {
        let mut intents = Vec::new();
        let reduced = reduced_motion(view.motion, view.system_reduced_motion);
        ui.style_mut().animation_time = if reduced { 0.0 } else { HOVER_SECONDS };
        ui.style_mut().scroll_animation = if reduced {
            egui::style::ScrollAnimation::none()
        } else {
            egui::style::ScrollAnimation::duration(HOVER_SECONDS)
        };
        let screen_changed = self
            .previous
            .is_none_or(|(screen, _)| screen != view.screen);
        let revision_changed = self
            .previous
            .is_some_and(|(_, revision)| revision != view.revision);
        if screen_changed {
            if self
                .previous
                .is_some_and(|(screen, _)| screen == ScreenId::Layout)
            {
                self.cancel_layout_drag();
            }
            self.transition_start = now_ms;
        }
        if revision_changed || (screen_changed && self.previous.is_some()) {
            // Retire pointer gestures too, including a drag whose screen stays open.
            self.cancel_layout_drag();
            // An Enter queued for an old plan must never activate a newly drawn control.
            ui.input_mut(|input| {
                input.events.retain(|event| {
                    !matches!(
                        event,
                        egui::Event::Key {
                            key: Key::Enter,
                            pressed: true,
                            ..
                        }
                    )
                });
            });
            if let Some(focused) = ui.memory(|memory| memory.focused()) {
                let retain_address = self.address_focus.iter().any(|(field, id)| {
                    *id == focused && view.fields.iter().any(|entry| matches!(entry, FieldView::PeerAddress { id, enabled: true, .. } if id == field))
                });
                if !retain_address {
                    ui.memory_mut(|memory| memory.surrender_focus(focused));
                }
            }
        }
        self.previous = Some((view.screen, view.revision));
        self.address_focus.clear();
        let fraction = transition_fraction(
            now_ms.saturating_sub(self.transition_start),
            TRANSITION_MS,
            reduced || !transition_allowed(view),
        );
        if fraction < 1.0 {
            ui.ctx().request_repaint_after(Duration::from_millis(16));
        }
        let permission_state = view
            .illustration
            .permission_row
            .and_then(|id| view.rows.iter().find(|row| row.id == id))
            .map(|row| row.state);
        let illustration_key = (
            view.screen,
            view.illustration.clone(),
            view.summary,
            permission_state,
            reduced,
        );
        if self.illustration_key.as_ref() != Some(&illustration_key) {
            self.illustration_key = Some(illustration_key);
            self.illustration_motion.reset(now_ms);
        }
        let (illustration_fraction, moving) = self.illustration_motion.advance(
            now_ms,
            illustration_duration(view),
            reduced,
            illustration_allowed(view),
        );
        if moving {
            ui.ctx().request_repaint_after(Duration::from_millis(16));
        }
        if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            match view.escape {
                EscapeMapping::None => {}
                EscapeMapping::Back => intents.push(WizardIntent::Back),
                EscapeMapping::Close => intents.push(WizardIntent::Close),
            }
        }
        art.background(ui.painter(), ui.max_rect());
        let wide = ui.ctx().viewport_rect().width() >= 1100.0;
        if !wide {
            ui.horizontal_wrapped(|ui| {
                ui.small("Setup progress");
                if let Some(group) = view.progress.current {
                    ui.label(
                        RichText::new(format!("Current: {}", progress_label(group)))
                            .color(theme::FROST),
                    );
                } else {
                    ui.small("Waiting for progress");
                }
                let completed = PROGRESS_GROUPS
                    .iter()
                    .filter(|group| view.progress.completed.contains(group))
                    .count();
                ui.small(format!("{completed} of 6 groups completed"));
            });
            ui.add_space(8.0);
        }
        ui.horizontal_top(|ui| {
            if wide {
                ui.vertical(|ui| {
                    ui.set_width(156.0);
                    ui.add_space(28.0);
                    art.emblem(ui, Vec2::splat(54.0));
                    ui.add_space(20.0);
                    ui.small("Setup progress");
                    for group in PROGRESS_GROUPS {
                        let current = Some(group) == view.progress.current;
                        let completed = view.progress.completed.contains(&group);
                        ui.horizontal(|ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                            if completed {
                                checkmark(ui.painter(), rect, theme::FROST, 1.0);
                            } else {
                                ui.painter().circle_stroke(
                                    rect.center(),
                                    4.0,
                                    (1.0, if current { theme::FROST } else { theme::QUIET }),
                                );
                            }
                            ui.label(RichText::new(progress_label(group)).color(if current {
                                theme::FROST
                            } else {
                                theme::QUIET
                            }));
                        });
                        if completed {
                            ui.small("Completed");
                        } else if current {
                            ui.small("Current");
                        }
                        ui.add_space(12.0);
                    }
                    ui.add_space(16.0);
                    ui.small("by Frostdev");
                });
                ui.add_space(18.0);
            }
            theme::glass().show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.scope(|ui| {
                            if view.screen == ScreenId::Welcome && illustration_allowed(view) {
                                ui.multiply_opacity(0.2 + 0.8 * illustration_fraction);
                            }
                            art.emblem(ui, Vec2::splat(34.0));
                        });
                        // The read-only compact wordmark is 689×96 pixels.
                        art.wordmark(ui, Vec2::new(689.0 / 3.0, 32.0));
                    });
                    if view.demo {
                        ui.label(RichText::new(DEMO_LABEL).color(theme::WARNING).strong());
                    }
                    ui.add_space(8.0);
                    ui.heading(&view.title);
                    ui.horizontal_wrapped(|ui| {
                        if let Some(machine) = &view.machine {
                            ui.small(machine);
                        }
                        if let Some(peer) = &view.peer {
                            ui.small(format!("↔ {peer}"));
                        }
                    });
                    ui.add_space(8.0);
                    // Reserve the footer before scrolling, so release/stop never scroll away.
                    let body_height = (ui.available_height() - 118.0).max(64.0);
                    let scroll_focus = ui
                        .make_persistent_id(("wizard-scroll-focus", format!("{:?}", view.screen)));
                    ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                    ui.spacing_mut().scroll.foreground_color = true;
                    let scroll = egui::ScrollArea::vertical()
                        .id_salt(("wizard-body", format!("{:?}", view.screen)))
                        .scroll_bar_visibility(
                            egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                        )
                        // Commit focus-reveal offsets in this pass, before the next paint.
                        // A zero-duration target still takes an extra pass when animated.
                        .animated(false)
                        .max_height(body_height)
                        .min_scrolled_height(body_height)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let response = ui.interact(
                                egui::Rect::from_min_size(
                                    ui.cursor().min,
                                    Vec2::new(ui.available_width(), 1.0),
                                ),
                                scroll_focus,
                                Sense::focusable_noninteractive(),
                            );
                            if response.has_focus() {
                                ui.memory_mut(|memory| {
                                    memory.set_focus_lock_filter(
                                        scroll_focus,
                                        egui::EventFilter {
                                            vertical_arrows: true,
                                            ..Default::default()
                                        },
                                    )
                                });
                                let delta = ui.input_mut(|input| {
                                    if input.consume_key(egui::Modifiers::NONE, Key::PageDown) {
                                        -body_height * 0.8
                                    } else if input.consume_key(egui::Modifiers::NONE, Key::PageUp)
                                    {
                                        body_height * 0.8
                                    } else if input
                                        .consume_key(egui::Modifiers::NONE, Key::ArrowDown)
                                    {
                                        -36.0
                                    } else if input.consume_key(egui::Modifiers::NONE, Key::ArrowUp)
                                    {
                                        36.0
                                    } else {
                                        0.0
                                    }
                                });
                                if delta != 0.0 {
                                    // egui computes directional focus before drawing. Retire
                                    // that movement as well as consuming the scrolling key.
                                    ui.memory_mut(|memory| {
                                        memory.move_focus(egui::FocusDirection::None)
                                    });
                                }
                                ui.scroll_with_delta(Vec2::new(0.0, delta));
                            }
                            ui.scope(|ui| {
                                ui.multiply_opacity(0.75 + 0.25 * fraction);
                                if !reduced {
                                    ui.add_space(8.0 * (1.0 - fraction));
                                }
                                let width = ui.available_width();
                                let padding = self
                                    .body_measure
                                    .as_ref()
                                    .filter(|measure| {
                                        measure.screen == view.screen
                                            && (measure.width - width).abs() < 0.5
                                            && (measure.viewport_height - body_height).abs() < 0.5
                                    })
                                    .map_or(0.0, |measure| {
                                        ((body_height - measure.content_height) * 0.5).max(0.0)
                                    });
                                ui.add_space(padding);
                                let start = ui.cursor().min.y;
                                self.body(
                                    ui,
                                    view,
                                    &mut intents,
                                    body_height,
                                    illustration_fraction,
                                );
                                // Measure the one real render, including rows and consent controls,
                                // at the post-scrollbar width. Never run interactive UI twice.
                                let content_height =
                                    (ui.cursor().min.y - start - ui.spacing().item_spacing.y)
                                        .max(0.0);
                                let next_padding = ((body_height - content_height) * 0.5).max(0.0);
                                if (next_padding - padding).abs() > 0.5 {
                                    ui.ctx().request_repaint();
                                }
                                self.body_measure = Some(BodyMeasure {
                                    screen: view.screen,
                                    width,
                                    viewport_height: body_height,
                                    content_height,
                                });
                            });
                        });
                    if ui.memory(|memory| memory.has_focus(scroll_focus)) {
                        ui.painter().rect_stroke(
                            scroll.inner_rect,
                            4.0,
                            (1.0, theme::FROST),
                            egui::StrokeKind::Inside,
                        );
                    }
                    ui.add_space(8.0);
                    ui.separator();
                    ui.push_id(("wizard-actions", view.revision), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            for button in &view.buttons {
                                let enabled = button.enabled
                                    && !(view.screen == ScreenId::HidingChoice
                                        && button.role == ButtonRole::Next
                                        && view.hiding_choice.is_none());
                                let response = ui
                                    .push_id(button.id, |ui| {
                                        ui.add_enabled_ui(enabled, |ui| match button.kind {
                                            ButtonKind::Primary => {
                                                theme::primary(ui, &button.label, enabled)
                                            }
                                            ButtonKind::Destructive => {
                                                theme::destructive(ui, &button.label)
                                            }
                                            ButtonKind::Secondary => ui.button(&button.label),
                                        })
                                        .inner
                                    })
                                    .inner;
                                if response.clicked() && enabled {
                                    intents.push(WizardIntent::Button(button.id));
                                }
                            }
                        });
                    });
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.small("Motion");
                        for (preference, label) in [
                            (MotionPreference::Auto, "Auto"),
                            (MotionPreference::Reduced, "Reduced"),
                            (MotionPreference::Full, "Full"),
                        ] {
                            if ui
                                .selectable_label(view.motion == preference, label)
                                .clicked()
                            {
                                intents.push(WizardIntent::SetMotion(preference));
                            }
                        }
                        if !wide {
                            ui.small("by Frostdev");
                        }
                    });
                });
            });
        });
        intents
            .into_iter()
            .map(|intent| WizardAction {
                revision: view.revision,
                intent,
            })
            .collect()
    }

    fn body(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        intents: &mut Vec<WizardIntent>,
        body_height: f32,
        illustration_fraction: f32,
    ) {
        let compact = body_height < 300.0;
        ui.spacing_mut().item_spacing.y = 6.0;
        let illustration_height = if !illustrated(view) {
            0.0
        } else if compact || matches!(view.screen, ScreenId::InstallPlan | ScreenId::Grants) {
            96.0
        } else {
            (body_height * 0.45).clamp(120.0, 420.0)
        };
        if compact {
            rows(ui, &view.rows);
        }
        ui.label(&view.message);
        ui.add_space(12.0);
        if view.screen == ScreenId::HidingChoice {
            for (choice, label) in [
                (HidingChoice::Hide, HIDE_LABEL),
                (HidingChoice::Mirror, MIRROR_LABEL),
            ] {
                ui.push_id(("hiding", view.revision, label), |ui| {
                    ui.visuals_mut().widgets.inactive.bg_stroke =
                        egui::Stroke::new(1.5, theme::GLACIER);
                    let response = ui.add(egui::RadioButton::new(
                        view.hiding_choice == Some(choice),
                        label,
                    ));
                    reveal_focus(&response);
                    if response.clicked() && ui.clip_rect().contains_rect(response.rect) {
                        intents.push(WizardIntent::ChooseHiding(choice));
                    }
                });
            }
        }
        if view.screen == ScreenId::AudioComponent
            && !view
                .rows
                .iter()
                .any(|row| row.detail.contains(MICROPHONE_COPY))
        {
            ui.label(MICROPHONE_DETAIL);
            ui.add_space(8.0);
        }
        if illustrated(view) {
            illustration(ui, view, illustration_height, illustration_fraction);
            ui.add_space(12.0);
        }
        if view.screen == ScreenId::Summary {
            let label = match view.summary {
                SummaryView::NotInstalled => "Not installed",
                SummaryView::InstalledWaiting => "Installed — waiting for checks",
                SummaryView::WorkspaceReady => "Workspace ready",
            };
            theme::section(ui, label);
        }
        if !compact {
            rows(ui, &view.rows);
        }
        for field in &view.fields {
            match field {
                FieldView::PeerAddress { id, value, enabled } => {
                    ui.label("Peer address");
                    let mut edited = bounded_address(value);
                    let response = ui.add_enabled(
                        *enabled,
                        egui::TextEdit::singleline(&mut edited)
                            .id(ui.make_persistent_id(("peer-address", id)))
                            .hint_text("Address of the other machine")
                            .desired_width(ui.available_width()),
                    );
                    self.address_focus.push((*id, response.id));
                    reveal_focus(&response);
                    if response.changed() {
                        intents.push(WizardIntent::EditPeerAddress {
                            field: *id,
                            value: bounded_address(&edited),
                        });
                    }
                }
                FieldView::Toggle {
                    id,
                    role,
                    label,
                    checked,
                    enabled,
                } => {
                    let mut value = *checked;
                    let label = if *role == ToggleRole::RemoveAudioDriver {
                        REMOVE_AUDIO_LABEL
                    } else {
                        label
                    };
                    let response = ui
                        .push_id(("toggle", view.revision, id), |ui| {
                            ui.visuals_mut().widgets.inactive.bg_stroke =
                                egui::Stroke::new(1.5, theme::GLACIER);
                            ui.add_enabled(*enabled, egui::Checkbox::new(&mut value, label))
                        })
                        .inner;
                    reveal_focus(&response);
                    if response.changed() && *enabled && ui.clip_rect().contains_rect(response.rect)
                    {
                        intents.push(WizardIntent::SetToggle {
                            field: *id,
                            checked: value,
                        });
                    }
                }
            }
            ui.add_space(4.0);
        }
        if view.screen == ScreenId::Layout {
            if let Some(layout) = &view.layout {
                self.layout.follow(&layout.confirmed);
                // The canvas receives finite bounds even inside a vertical scroll area.
                let action = ui
                    .push_id(("layout", view.revision), |ui| {
                        ui.allocate_ui_with_layout(
                            Vec2::new(ui.available_width(), 340.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                // The kit's first row is the Apply/Revert toolbar. Prevent
                                // keyboard activation before calling it: Revert mutates its
                                // local editor, so rejecting a returned action is too late.
                                let toolbar_height = ui.spacing().interact_size.y.max(
                                    ui.text_style_height(&egui::TextStyle::Button)
                                        + 2.0 * ui.spacing().button_padding.y,
                                );
                                let toolbar = egui::Rect::from_min_size(
                                    ui.cursor().min,
                                    Vec2::new(ui.available_width(), toolbar_height),
                                );
                                let withheld_keys = if ui.clip_rect().contains_rect(toolbar) {
                                    Vec::new()
                                } else {
                                    ui.input_mut(|input| {
                                        let (withheld, remaining) =
                                            std::mem::take(&mut input.events)
                                                .into_iter()
                                                .partition(|event| {
                                                    matches!(
                                                        event,
                                                        egui::Event::Key {
                                                            key: Key::Enter | Key::Space,
                                                            ..
                                                        }
                                                    )
                                                });
                                        input.events = remaining;
                                        withheld
                                    })
                                };
                                let action = self.layout.show(
                                    ui,
                                    crosspane_ui_kit::layout::LayoutView {
                                        confirmed: &layout.confirmed,
                                        local_node: &layout.local_node,
                                        peer_order: &layout.peer_order,
                                        busy: layout.busy,
                                        feedback: None,
                                    },
                                );
                                // Footer activation remains available; the suppression applies
                                // only while the clipped kit toolbar is handling input.
                                ui.input_mut(|input| input.events.extend(withheld_keys));
                                if let Some(response) = ui
                                    .memory(|memory| memory.focused())
                                    .and_then(|id| ui.ctx().read_response(id))
                                    && ui.min_rect().contains(response.rect.center())
                                {
                                    reveal_focus(&response);
                                }
                                action
                            },
                        )
                        .inner
                    })
                    .inner;
                if let Some(action) = action {
                    // Revert has already reset both kit editor and canvas state.
                    intents.push(WizardIntent::Layout(action));
                }
            } else {
                ui.label("Waiting for confirmed display positions");
            }
        }
    }
}

fn reveal_focus(response: &egui::Response) {
    if response.has_focus()
        && (response.gained_focus() || !response.interact_rect.contains_rect(response.rect))
    {
        // Focused consent must be readable on the next paint even in Full motion;
        // using the context's default scroll animation would also bypass Reduced.
        response.scroll_to_me_animation(
            Some(egui::Align::Center),
            egui::style::ScrollAnimation::none(),
        );
    }
}

fn rows(ui: &mut egui::Ui, rows: &[RowView]) {
    for row in rows {
        let (label, color, icon) = row_style(row.state);
        theme::glass().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
                theme::icon(ui.painter(), rect, icon, color);
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(&row.label).strong());
                    ui.label(RichText::new(label).color(color));
                    ui.label(&row.detail);
                    if row.human_confirmed {
                        ui.small("Confirmed by you");
                    }
                });
            });
        });
        ui.add_space(6.0);
    }
}

const PROGRESS_GROUPS: [ProgressGroup; 6] = [
    ProgressGroup::Install,
    ProgressGroup::PermissionsNetwork,
    ProgressGroup::Connect,
    ProgressGroup::Arrange,
    ProgressGroup::Practice,
    ProgressGroup::Ready,
];

fn progress_label(group: ProgressGroup) -> &'static str {
    match group {
        ProgressGroup::Install => "Install",
        ProgressGroup::PermissionsNetwork => "Permissions / Network",
        ProgressGroup::Connect => "Connect",
        ProgressGroup::Arrange => "Arrange",
        ProgressGroup::Practice => "Practice",
        ProgressGroup::Ready => "Ready",
    }
}

fn illustrated(view: &WizardView) -> bool {
    matches!(
        view.screen,
        ScreenId::Welcome
            | ScreenId::InstallPlan
            | ScreenId::Grants
            | ScreenId::AudioComponent
            | ScreenId::Network
            | ScreenId::HidingChoice
            | ScreenId::MatchNumbers
            | ScreenId::Summary
    ) || (view.screen == ScreenId::Permissions && view.illustration.permission_row.is_some())
        || (view.screen == ScreenId::Practice && view.illustration.practice.is_some())
}

fn checkmark(painter: &egui::Painter, rect: egui::Rect, color: Color32, fraction: f32) {
    let rect = rect.shrink(rect.width() * 0.15);
    let points = [
        egui::pos2(rect.left(), rect.center().y),
        egui::pos2(rect.center().x - rect.width() * 0.1, rect.bottom()),
        egui::pos2(rect.right(), rect.top()),
    ];
    painter.add(egui::Shape::line(
        points.to_vec(),
        (
            2.0,
            theme::alpha(color, (255.0 * fraction.clamp(0.0, 1.0)) as u8),
        ),
    ));
}

fn display_glyph(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    painter.rect_filled(rect, 8.0, theme::alpha(theme::NAVY, 160));
    painter.rect_stroke(rect, 8.0, (2.0, color), egui::StrokeKind::Inside);
    painter.line_segment(
        [
            rect.center_bottom(),
            rect.center_bottom() + egui::vec2(0.0, 10.0),
        ],
        (2.0, color),
    );
    painter.line_segment(
        [
            rect.center_bottom() + egui::vec2(-16.0, 11.0),
            rect.center_bottom() + egui::vec2(16.0, 11.0),
        ],
        (2.0, color),
    );
}

fn window_glyph(painter: &egui::Painter, rect: egui::Rect) {
    painter.rect_filled(rect, 4.0, theme::alpha(theme::FROST, 75));
    painter.rect_stroke(rect, 4.0, (1.5, theme::GLACIER), egui::StrokeKind::Inside);
    painter.line_segment(
        [
            rect.left_top() + egui::vec2(3.0, 10.0),
            rect.right_top() + egui::vec2(-3.0, 10.0),
        ],
        (1.0, theme::ICE),
    );
}

fn caption(painter: &egui::Painter, rect: egui::Rect, text: &str, size: f32, color: Color32) {
    let galley = painter.layout(
        text.into(),
        egui::FontId::proportional(size),
        color,
        rect.width(),
    );
    painter.galley(
        egui::pos2(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        color,
    );
}

fn arrow(painter: &egui::Painter, from: egui::Pos2, to: egui::Pos2, color: Color32) {
    painter.arrow(from, to - from, (2.0, color));
}

fn dashed_display(painter: &egui::Painter, rect: egui::Rect) {
    painter.rect_filled(rect, 8.0, theme::alpha(theme::NAVY, 70));
    for (from, to) in [
        (rect.left_top(), rect.right_top()),
        (rect.right_top(), rect.right_bottom()),
        (rect.right_bottom(), rect.left_bottom()),
        (rect.left_bottom(), rect.left_top()),
    ] {
        let length = from.distance(to);
        let direction = (to - from).normalized();
        let mut distance = 0.0;
        while distance < length {
            painter.line_segment(
                [
                    from + direction * distance,
                    from + direction * (distance + 7.0).min(length),
                ],
                (1.5, theme::GLACIER),
            );
            distance += 13.0;
        }
    }
}

fn illustration(ui: &mut egui::Ui, view: &WizardView, height: f32, fraction: f32) {
    let (bounds, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let painter = ui.painter_at(bounds);
    theme::gradient(
        &painter,
        bounds,
        14.0,
        theme::alpha(theme::NAVY, 120),
        theme::alpha(theme::MIDNIGHT, 180),
    );
    let compact = height <= 120.0;
    let content = bounds.shrink2(if compact {
        egui::vec2(12.0, 10.0)
    } else {
        egui::vec2(20.0, 18.0)
    });
    let note_height = if compact { 24.0 } else { 32.0 };
    let diagram = egui::Rect::from_min_max(
        content.min,
        content.max - egui::vec2(0.0, note_height + 4.0),
    );
    let size = egui::vec2(
        (content.width() * 0.23).min(150.0),
        (diagram.height() * 0.60).min(100.0),
    );
    let centre = diagram.center();
    let left = egui::Rect::from_center_size(centre - egui::vec2(size.x * 0.8 + 22.0, 0.0), size);
    let right = egui::Rect::from_center_size(centre + egui::vec2(size.x * 0.8 + 22.0, 0.0), size);
    let note = egui::Rect::from_min_max(
        egui::pos2(content.left(), content.bottom() - note_height),
        content.right_bottom(),
    );
    match view.screen {
        ScreenId::InstallPlan => {
            let width = (content.width() * 0.24).min(165.0);
            for (offset, label, color) in [
                (-1.25, "User agent", theme::FROST),
                (0.0, "User startup", theme::GLACIER),
                (1.25, "Audio driver", theme::QUIET),
            ] {
                let rect = egui::Rect::from_center_size(
                    centre + egui::vec2(offset * width, 0.0),
                    egui::vec2(width, size.y.max(30.0)),
                );
                painter.rect_filled(rect, 6.0, theme::alpha(theme::NAVY, 180));
                painter.rect_stroke(rect, 6.0, (1.5, color), egui::StrokeKind::Inside);
                caption(&painter, rect.shrink(3.0), label, 13.0, theme::ICE);
            }
            caption(
                &painter,
                note,
                "Preview: user-owned agent and startup; optional audio package affects all users",
                13.0,
                theme::QUIET,
            );
        }
        ScreenId::Grants => {
            display_glyph(&painter, left, theme::FROST);
            display_glyph(&painter, right, theme::PEER_ICE);
            caption(&painter, left.shrink(3.0), "This machine", 12.0, theme::ICE);
            caption(
                &painter,
                right.shrink(3.0),
                "Other machine",
                12.0,
                theme::ICE,
            );
            arrow(
                &painter,
                left.right_center() + egui::vec2(6.0, -6.0),
                right.left_center() + egui::vec2(-6.0, -6.0),
                theme::QUIET,
            );
            arrow(
                &painter,
                right.left_center() + egui::vec2(-6.0, 6.0),
                left.right_center() + egui::vec2(6.0, 6.0),
                theme::QUIET,
            );
            caption(
                &painter,
                note,
                "Control and windows · each direction is a separate opt-in",
                13.0,
                theme::QUIET,
            );
        }
        ScreenId::Welcome => {
            let gap = 34.0 * (1.0 - fraction);
            let a =
                egui::Rect::from_center_size(centre - egui::vec2(size.x * 0.5 + gap, 0.0), size);
            let b =
                egui::Rect::from_center_size(centre + egui::vec2(size.x * 0.5 + gap, 0.0), size);
            display_glyph(&painter, a, theme::FROST);
            display_glyph(&painter, b, theme::PEER_ICE);
            if fraction >= 0.95 {
                theme::crossing_glow(&painter, [a.right_top(), a.right_bottom()], true);
            }
            caption(
                &painter,
                note,
                "Keyboard, windows and speakers across your computers",
                14.0,
                theme::ICE,
            );
        }
        ScreenId::Permissions => {
            let row = view
                .illustration
                .permission_row
                .and_then(|id| view.rows.iter().find(|row| row.id == id));
            let panel = egui::Rect::from_center_size(
                centre,
                egui::vec2(
                    content.width().min(390.0),
                    size.y.max(if compact { 38.0 } else { 72.0 }),
                ),
            );
            painter.rect_filled(panel, 10.0, theme::alpha(theme::NAVY, 220));
            painter.rect_stroke(panel, 10.0, (1.0, theme::QUIET), egui::StrokeKind::Inside);
            let track = egui::Rect::from_center_size(
                panel.right_center() - egui::vec2(46.0, 0.0),
                egui::vec2(46.0, 24.0),
            );
            let verified = row.is_some_and(|row| row.state == RowState::Verified);
            painter.rect_filled(
                track,
                12.0,
                theme::alpha(if verified { theme::FROST } else { theme::QUIET }, 90),
            );
            painter.circle_filled(
                track.center() + egui::vec2(if verified { 10.0 } else { -10.0 }, 0.0),
                8.0,
                theme::ICE,
            );
            let pulse = (fraction * std::f32::consts::PI).sin().max(0.0);
            painter.rect_stroke(
                track.expand(4.0 + 3.0 * pulse),
                16.0,
                (
                    1.0 + pulse,
                    theme::alpha(theme::FROST, (80.0 + 150.0 * pulse) as u8),
                ),
                egui::StrokeKind::Outside,
            );
            if verified {
                checkmark(
                    &painter,
                    egui::Rect::from_center_size(track.center(), Vec2::splat(20.0)),
                    theme::MIDNIGHT,
                    1.0,
                );
            }
            let label_rect = egui::Rect::from_min_max(
                panel.left_top() + egui::vec2(12.0, 8.0),
                panel.right_bottom() - egui::vec2(90.0, 8.0),
            );
            caption(
                &painter,
                label_rect,
                row.map_or("Waiting for current permission", |row| row.label.as_str()),
                if compact { 14.0 } else { 17.0 },
                theme::ICE,
            );
            caption(
                &painter,
                note,
                "Illustrative Settings guide · system permission remains your choice",
                13.0,
                theme::QUIET,
            );
        }
        ScreenId::AudioComponent => {
            let width = (content.width() * 0.2).min(125.0);
            let middle = egui::Rect::from_center_size(centre, egui::vec2(width, size.y));
            let a =
                egui::Rect::from_center_size(centre - egui::vec2(width * 1.7, 0.0), middle.size());
            let b =
                egui::Rect::from_center_size(centre + egui::vec2(width * 1.7, 0.0), middle.size());
            display_glyph(&painter, a, theme::QUIET);
            painter.rect_filled(middle, 8.0, theme::alpha(theme::FROST, 40));
            painter.rect_stroke(middle, 8.0, (2.0, theme::FROST), egui::StrokeKind::Inside);
            display_glyph(&painter, b, theme::PEER_ICE);
            arrow(
                &painter,
                a.right_center() + egui::vec2(5.0, 0.0),
                middle.left_center() - egui::vec2(5.0, 0.0),
                theme::FROST,
            );
            arrow(
                &painter,
                middle.right_center() + egui::vec2(5.0, 0.0),
                b.left_center() - egui::vec2(5.0, 0.0),
                theme::FROST,
            );
            caption(&painter, a.shrink(3.0), "Output audio", 12.0, theme::ICE);
            caption(
                &painter,
                middle.shrink(3.0),
                "Crosspane speakers",
                12.0,
                theme::ICE,
            );
            caption(&painter, b.shrink(3.0), "Peer playback", 12.0, theme::ICE);
            caption(
                &painter,
                note,
                "Virtual speakers loopback · no physical microphone",
                13.0,
                theme::QUIET,
            );
        }
        ScreenId::Network => {
            display_glyph(&painter, left, theme::FROST);
            display_glyph(&painter, right, theme::PEER_ICE);
            let observed = view.illustration.traffic_observed;
            painter.line_segment(
                [left.right_center(), right.left_center()],
                (
                    if observed { 3.0 } else { 1.0 },
                    if observed { theme::FROST } else { theme::QUIET },
                ),
            );
            if observed {
                painter.circle_filled(centre, 5.0 + 2.0 * fraction, theme::GLACIER);
            }
            caption(
                &painter,
                note,
                if observed {
                    "Traffic observed"
                } else {
                    "Waiting for observed traffic"
                },
                14.0,
                if observed { theme::FROST } else { theme::QUIET },
            );
        }
        ScreenId::HidingChoice => {
            display_glyph(&painter, left, theme::QUIET);
            dashed_display(&painter, right);
            let position = left.center().lerp(right.center(), fraction);
            window_glyph(
                &painter,
                egui::Rect::from_center_size(position, size * 0.65),
            );
            caption(
                &painter,
                note,
                "Hide preview · separate virtual display; Mirror keeps windows here and is the fallback",
                13.0,
                theme::QUIET,
            );
        }
        ScreenId::MatchNumbers => {
            let digits = view
                .illustration
                .sas
                .as_deref()
                .unwrap_or("Waiting for numbers");
            caption(
                &painter,
                content.shrink2(egui::vec2(12.0, 24.0)),
                digits,
                42.0,
                theme::ICE,
            );
            caption(
                &painter,
                note,
                "Compare on both machines before you confirm",
                13.0,
                theme::QUIET,
            );
        }
        ScreenId::Practice => {
            display_glyph(&painter, left, theme::FROST);
            display_glyph(&painter, right, theme::PEER_ICE);
            match view.illustration.practice {
                Some(PracticeIllustration::Pointer) => {
                    let point = left.center().lerp(right.center(), fraction);
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            point,
                            point + egui::vec2(0.0, 22.0),
                            point + egui::vec2(16.0, 15.0),
                        ],
                        theme::ICE,
                        egui::Stroke::new(1.0, theme::MIDNIGHT),
                    ));
                }
                Some(PracticeIllustration::Window) => window_glyph(
                    &painter,
                    egui::Rect::from_center_size(
                        left.center().lerp(right.center(), fraction),
                        size * 0.6,
                    ),
                ),
                Some(PracticeIllustration::Tone) => {
                    for index in 0..5 {
                        let x = centre.x - 24.0 + index as f32 * 12.0;
                        let amplitude = 10.0
                            + (fraction * std::f32::consts::PI).sin().abs()
                                * (10.0 + index as f32 * 3.0);
                        painter.line_segment(
                            [
                                egui::pos2(x, centre.y - amplitude),
                                egui::pos2(x, centre.y + amplitude),
                            ],
                            (3.0, theme::FROST),
                        );
                    }
                }
                None => {}
            }
            caption(
                &painter,
                note,
                "Illustration only · practice needs evidence and your confirmation",
                13.0,
                theme::QUIET,
            );
        }
        ScreenId::Summary => {
            let verified = view
                .rows
                .iter()
                .filter(|row| row.state == RowState::Verified)
                .count()
                .min(6);
            if view.summary == SummaryView::WorkspaceReady && verified > 0 {
                for index in 0..verified {
                    let centre = centre
                        + egui::vec2(
                            (index as f32 - (verified - 1) as f32 * 0.5) * 52.0,
                            6.0 * (1.0 - fraction),
                        );
                    let rect = egui::Rect::from_center_size(centre, Vec2::splat(34.0));
                    painter.circle_stroke(centre, 22.0, (1.5, theme::FROST));
                    checkmark(&painter, rect, theme::FROST, 0.5 + 0.5 * fraction);
                }
                caption(&painter, note, "Verified evidence", 14.0, theme::FROST);
            } else {
                caption(
                    &painter,
                    content,
                    "Waiting for verification",
                    20.0,
                    theme::QUIET,
                );
            }
        }
        _ => {}
    }
}

fn bounded_address(value: &str) -> String {
    let mut end = value.len().min(512);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].replace(['\r', '\n'], "")
}

fn row_style(state: RowState) -> (&'static str, Color32, usize) {
    match state {
        RowState::Unchecked => ("Not checked", theme::QUIET, 0),
        RowState::Working => ("Working", theme::FROST, 1),
        RowState::NeedsAction => ("Needs your action", theme::WARNING, 2),
        RowState::Waiting => ("Waiting", theme::QUIET, 3),
        RowState::Verified => ("Verified", theme::FROST, 4),
        RowState::Failed => ("Failed", theme::WARNING, 5),
        RowState::Unsupported => ("Unsupported", theme::WARNING, 6),
    }
}

#[cfg(test)]
mod tests {
    use super::bounded_address;

    #[test]
    fn address_bound_preserves_utf8_and_single_line() {
        let address = "é".repeat(400);
        assert_eq!(bounded_address(&address).len(), 512);
        assert_eq!(bounded_address("one\r\ntwo"), "onetwo");
    }
}
