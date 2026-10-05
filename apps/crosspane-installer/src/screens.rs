//! The wizard shell: one card that holds the current question, with progress beside it (wide
//! windows) or above it (narrow ones).
//!
//! Layout rules, so every screen reads the same way:
//! - The card is as tall as its content, never padded out; long content scrolls inside the card
//!   while the footer stays put.
//! - A screen has at most one filled (primary) button. Secondary actions are quiet links under
//!   the content; answers to pick from are tiles in the content.
//! - Footer buttons never wrap mid-word: they move to another row instead.
//! - A step reads as a mark plus plain words: done, in progress, waiting, or a problem with its
//!   fix. No status vocabulary.

use std::time::Duration;

use crosspane_ui_kit::theme::StepMark;
use crosspane_ui_kit::{art::Art, layout::LayoutWidget, theme};
use eframe::egui::{self, Color32, FontId, Id, Key, RichText, Sense, TextStyle, Vec2};

use crate::demo::{
    DEMO_LABEL, HIDE_LABEL, MICROPHONE_COPY, MICROPHONE_DETAIL, MIRROR_LABEL, REMOVE_AUDIO_LABEL,
};
use crate::motion::{
    HOVER_SECONDS, IllustrationMotion, TRANSITION_MS, illustration_allowed, illustration_duration,
    transition_allowed,
};
use crate::view::*;
use crate::{reduced_motion, transition_fraction};

/// From this viewport width on, progress is a rail beside the card.
const RAIL_BREAKPOINT: f32 = 960.0;
const RAIL_WIDTH: f32 = 196.0;
const RAIL_GAP: f32 = 28.0;
const CARD_MAX_WIDTH: f32 = 760.0;
const CARD_MARGIN: egui::Margin = egui::Margin {
    left: 28,
    right: 28,
    top: 24,
    bottom: 22,
};
/// One turn of the in-progress mark, when motion is allowed.
const SPIN_MS: u64 = 1200;

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
    /// The small motion-settings panel is open.
    settings_open: bool,
    /// Where this frame's layout placed the settings gear.
    settings_rect: Option<egui::Rect>,
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

/// Screens that read as one page. The three install screens share a title and a checklist, so
/// moving between them by itself is not a page change.
fn page_of(screen: ScreenId) -> ScreenId {
    match screen {
        ScreenId::Compatibility | ScreenId::InstallPlan | ScreenId::Installing => {
            ScreenId::Installing
        }
        other => other,
    }
}

/// Everything the card needs that was decided before it is drawn.
struct FrameState {
    reduced: bool,
    fraction: f32,
    illustration_fraction: f32,
    /// The turn of in-progress marks, or `None` to draw them still.
    phase: Option<f32>,
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
        {
            let style = ui.style_mut();
            style.animation_time = if reduced { 0.0 } else { HOVER_SECONDS };
            style.scroll_animation = if reduced {
                egui::style::ScrollAnimation::none()
            } else {
                egui::style::ScrollAnimation::duration(HOVER_SECONDS)
            };
            // The installer's type scale: readable body text and one clear title size.
            style
                .text_styles
                .insert(TextStyle::Body, FontId::proportional(15.0));
            style
                .text_styles
                .insert(TextStyle::Button, FontId::proportional(15.0));
            style
                .text_styles
                .insert(TextStyle::Heading, FontId::proportional(26.0));
            style
                .text_styles
                .insert(TextStyle::Small, FontId::proportional(12.0));
            style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        }
        let screen_changed = self
            .previous
            .is_none_or(|(screen, _)| screen != view.screen);
        let page_changed = self
            .previous
            .is_none_or(|(screen, _)| page_of(screen) != page_of(view.screen));
        let revision_changed = self
            .previous
            .is_some_and(|(_, revision)| revision != view.revision);
        if screen_changed
            && self
                .previous
                .is_some_and(|(screen, _)| screen == ScreenId::Layout)
        {
            self.cancel_layout_drag();
        }
        if page_changed {
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
        // Work in progress turns its mark only when motion is allowed; reduced motion draws it
        // still and asks for no repaint.
        let spinning = !reduced && view.rows.iter().any(|row| row.state == RowState::Working);
        let phase = spinning.then(|| (now_ms % SPIN_MS) as f32 / SPIN_MS as f32);
        if spinning {
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }
        if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            match view.escape {
                EscapeMapping::None => {}
                EscapeMapping::Back => intents.push(WizardIntent::Back),
                EscapeMapping::Close => intents.push(WizardIntent::Close),
            }
        }
        let viewport = ui.ctx().viewport_rect();
        art.background(&ui.painter().with_clip_rect(viewport), viewport);
        let frame = FrameState {
            reduced,
            fraction,
            illustration_fraction,
            phase,
        };
        let wide = viewport.width() >= RAIL_BREAKPOINT;
        if wide {
            // Large windows hold the rail and card a little lower, by a fixed share of the spare
            // height (not of the content's, so nothing jumps when a screen grows), and keep them
            // together as one group, centred.
            ui.add_space(((ui.available_height() - 680.0) * 0.3).max(0.0));
            let group = RAIL_WIDTH + RAIL_GAP + CARD_MAX_WIDTH;
            let inset = ((ui.available_width() - group) * 0.5).max(0.0);
            ui.horizontal_top(|ui| {
                ui.add_space(inset);
                self.rail(ui, view, art, &mut intents);
                ui.add_space(RAIL_GAP);
                self.column(ui, view, &mut intents, &frame, false);
            });
        } else {
            self.top_bar(ui, view, art, &mut intents);
            ui.add_space(14.0);
            self.column(ui, view, &mut intents, &frame, true);
        }
        self.settings_button(ui);
        intents
            .into_iter()
            .map(|intent| WizardAction {
                revision: view.revision,
                intent,
            })
            .collect()
    }

    /// Wide windows: brand, the six progress groups and the settings affordance.
    fn rail(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        art: &Art,
        intents: &mut Vec<WizardIntent>,
    ) {
        ui.vertical(|ui| {
            ui.set_width(RAIL_WIDTH);
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                art.emblem(ui, Vec2::splat(34.0));
                // The read-only compact wordmark is 689×96 pixels.
                art.wordmark(ui, Vec2::new(689.0 / 96.0 * 17.0, 17.0));
            });
            ui.add_space(34.0);
            let groups = PROGRESS_GROUPS.len();
            for (index, group) in PROGRESS_GROUPS.into_iter().enumerate() {
                let current = view.progress.current == Some(group);
                let done = view.progress.completed.contains(&group);
                let label = progress_label(group);
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(RAIL_WIDTH, 40.0), Sense::hover());
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Label,
                        true,
                        format!(
                            "{label}: {}",
                            if done {
                                "done"
                            } else if current {
                                "current step"
                            } else {
                                "not started"
                            }
                        ),
                    )
                });
                let mark_rect = egui::Rect::from_center_size(
                    rect.left_center() + Vec2::new(10.0, 0.0),
                    Vec2::splat(18.0),
                );
                if index + 1 < groups {
                    ui.painter().line_segment(
                        [
                            mark_rect.center_bottom() + Vec2::new(0.0, 4.0),
                            mark_rect.center_bottom() + Vec2::new(0.0, 26.0),
                        ],
                        (1.0, theme::alpha(theme::QUIET, if done { 120 } else { 45 })),
                    );
                }
                let mark = if done {
                    StepMark::Done
                } else if current {
                    StepMark::Current
                } else {
                    StepMark::Pending
                };
                theme::step_mark(ui.painter(), mark_rect, mark, None);
                let color = if current {
                    theme::ICE
                } else if done {
                    theme::QUIET
                } else {
                    theme::alpha(theme::QUIET, 170)
                };
                ui.painter().text(
                    rect.left_center() + Vec2::new(32.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    label,
                    FontId::proportional(if current { 15.5 } else { 14.5 }),
                    color,
                );
            }
            ui.add_space(26.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("by Frostdev")
                        .size(12.0)
                        .color(theme::alpha(theme::QUIET, 200)),
                );
                ui.add_space((ui.available_width() - 40.0).max(0.0));
                self.reserve_settings_button(ui);
            });
            self.settings_panel(ui, view, intents);
        });
    }

    /// Narrow windows: brand and settings on one line, then a segmented progress strip.
    fn top_bar(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        art: &Art,
        intents: &mut Vec<WizardIntent>,
    ) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            art.emblem(ui, Vec2::splat(28.0));
            art.wordmark(ui, Vec2::new(689.0 / 96.0 * 15.0, 15.0));
            ui.add_space((ui.available_width() - 36.0).max(0.0));
            self.reserve_settings_button(ui);
        });
        self.settings_panel(ui, view, intents);
        ui.add_space(8.0);
        let width = ui.available_width().min(CARD_MAX_WIDTH);
        let (strip, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
        let strip = egui::Rect::from_center_size(strip.center(), Vec2::new(width, 4.0));
        let done: Vec<bool> = PROGRESS_GROUPS
            .iter()
            .map(|group| view.progress.completed.contains(group))
            .collect();
        let current = view
            .progress
            .current
            .and_then(|group| PROGRESS_GROUPS.iter().position(|g| *g == group));
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Label,
                true,
                format!(
                    "Setup progress: {} of {} done",
                    done.iter().filter(|d| **d).count(),
                    PROGRESS_GROUPS.len()
                ),
            )
        });
        theme::progress_strip(ui.painter(), strip, &done, current);
    }

    /// Holds the gear's place in the layout. The button itself is made after the card, so it
    /// comes last in keyboard order and never stands between the person and the content.
    fn reserve_settings_button(&mut self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
        self.settings_rect = Some(rect);
    }

    fn settings_button(&mut self, ui: &mut egui::Ui) {
        let Some(rect) = self.settings_rect.take() else {
            return;
        };
        let response = ui.interact(
            rect,
            ui.make_persistent_id("motion-settings"),
            Sense::click(),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Motion settings")
        });
        let lit = response.hovered() || response.has_focus() || self.settings_open;
        theme::gear(
            ui.painter(),
            rect.shrink(7.0),
            if lit {
                theme::GLACIER
            } else {
                theme::alpha(theme::QUIET, 190)
            },
        );
        if response.has_focus() {
            ui.painter()
                .rect_stroke(rect, 8.0, (1.5, theme::GLACIER), egui::StrokeKind::Inside);
        }
        if response.clicked() {
            self.settings_open = !self.settings_open;
        }
        response.on_hover_text("Motion settings");
    }

    fn settings_panel(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        intents: &mut Vec<WizardIntent>,
    ) {
        if !self.settings_open {
            return;
        }
        egui::Frame::new()
            .fill(theme::alpha(theme::NAVY, 90))
            .stroke((1.0, theme::alpha(theme::GLACIER, 45)))
            .corner_radius(10)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.label(RichText::new("Motion").size(12.5).color(theme::QUIET));
                ui.horizontal(|ui| {
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
                });
            });
    }

    /// The card at a readable width: next to the rail, or centred when it stands alone.
    fn column(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        intents: &mut Vec<WizardIntent>,
        frame: &FrameState,
        centred: bool,
    ) {
        let available = ui.available_width();
        let width = available.min(CARD_MAX_WIDTH);
        ui.horizontal_top(|ui| {
            if centred {
                ui.add_space(((available - width) * 0.5).max(0.0));
            }
            ui.vertical(|ui| {
                ui.set_width(width);
                self.card(ui, view, intents, frame);
            });
        });
    }

    fn card(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        intents: &mut Vec<WizardIntent>,
        frame: &FrameState,
    ) {
        let max_height = ui.available_height();
        theme::glass().inner_margin(CARD_MARGIN).show(ui, |ui| {
            let inner = ui.available_width();
            ui.set_width(inner);
            let top = ui.cursor().min.y;
            header(ui, view);
            ui.add_space(14.0);
            let plan = FooterPlan::new(ui, view, inner);
            let used = ui.cursor().min.y - top;
            let margins = f32::from(CARD_MARGIN.top) + f32::from(CARD_MARGIN.bottom);
            let footer = if plan.is_empty() {
                0.0
            } else {
                plan.height + 18.0
            };
            let body_height = (max_height - margins - used - footer - 4.0).max(96.0);
            self.scroll_body(ui, view, intents, frame, body_height);
            if !plan.is_empty() {
                ui.add_space(10.0);
                plan.show(ui, view, intents);
            }
        });
    }

    fn scroll_body(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        intents: &mut Vec<WizardIntent>,
        frame: &FrameState,
        body_height: f32,
    ) {
        let scroll_focus =
            ui.make_persistent_id(("wizard-scroll-focus", format!("{:?}", view.screen)));
        ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
        ui.spacing_mut().scroll.foreground_color = true;
        let scroll = egui::ScrollArea::vertical()
            .id_salt(("wizard-body", format!("{:?}", view.screen)))
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
            // Commit focus-reveal offsets in this pass, before the next paint.
            // A zero-duration target still takes an extra pass when animated.
            .animated(false)
            .max_height(body_height)
            // As tall as the content and no taller: no dead space above the footer.
            .auto_shrink([false, true])
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
                        } else if input.consume_key(egui::Modifiers::NONE, Key::PageUp) {
                            body_height * 0.8
                        } else if input.consume_key(egui::Modifiers::NONE, Key::ArrowDown) {
                            -36.0
                        } else if input.consume_key(egui::Modifiers::NONE, Key::ArrowUp) {
                            36.0
                        } else {
                            0.0
                        }
                    });
                    if delta != 0.0 {
                        // egui computes directional focus before drawing. Retire that movement
                        // as well as consuming the scrolling key.
                        ui.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
                    }
                    ui.scroll_with_delta(Vec2::new(0.0, delta));
                }
                ui.scope(|ui| {
                    ui.multiply_opacity(0.75 + 0.25 * frame.fraction);
                    if !frame.reduced {
                        ui.add_space(6.0 * (1.0 - frame.fraction));
                    }
                    self.body(ui, view, intents, frame, body_height);
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
    }

    fn body(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        intents: &mut Vec<WizardIntent>,
        frame: &FrameState,
        body_height: f32,
    ) {
        ui.spacing_mut().item_spacing.y = 8.0;
        if !view.message.is_empty() {
            ui.add(egui::Label::new(message_job(ui, &view.message)).wrap());
            ui.add_space(6.0);
        }
        if view.screen == ScreenId::HidingChoice {
            let width = ui.available_width();
            for (choice, label) in [
                (HidingChoice::Hide, HIDE_LABEL),
                (HidingChoice::Mirror, MIRROR_LABEL),
            ] {
                ui.push_id(("hiding", view.revision, label), |ui| {
                    let response =
                        theme::option(ui, view.hiding_choice == Some(choice), label, width);
                    reveal_focus(&response);
                    if response.clicked() && ui.clip_rect().contains_rect(response.rect) {
                        intents.push(WizardIntent::ChooseHiding(choice));
                    }
                });
            }
            ui.add_space(4.0);
        }
        if view.screen == ScreenId::AudioComponent
            && !view
                .rows
                .iter()
                .any(|row| row.detail.contains(MICROPHONE_COPY))
        {
            ui.label(RichText::new(MICROPHONE_DETAIL).color(theme::QUIET));
            ui.add_space(4.0);
        }
        if illustrated(view) {
            illustration(
                ui,
                view,
                illustration_height(view, body_height),
                frame.illustration_fraction,
            );
            ui.add_space(8.0);
        }
        if !view.rows.is_empty() {
            rows(ui, &view.rows, frame.phase);
            ui.add_space(4.0);
        }
        self.fields(ui, view, intents);
        if view.screen == ScreenId::Layout {
            self.layout(ui, view, intents);
        }
        choices(ui, view, intents);
        links(ui, view, intents);
    }

    fn fields(&mut self, ui: &mut egui::Ui, view: &WizardView, intents: &mut Vec<WizardIntent>) {
        let width = ui.available_width();
        for field in &view.fields {
            match field {
                FieldView::PeerAddress { id, value, enabled } => {
                    ui.label(
                        RichText::new("The other computer's address")
                            .size(13.5)
                            .color(theme::QUIET),
                    );
                    let mut edited = bounded_address(value);
                    let response = ui
                        .scope(|ui| {
                            let visuals = ui.visuals_mut();
                            visuals.weak_text_color = Some(theme::QUIET);
                            visuals.selection.stroke = egui::Stroke::new(1.0, theme::FROST);
                            visuals.widgets.inactive.bg_stroke =
                                egui::Stroke::new(1.0, theme::alpha(theme::GLACIER, 110));
                            visuals.widgets.hovered.bg_stroke =
                                egui::Stroke::new(1.0, theme::GLACIER);
                            ui.add_enabled(
                                *enabled,
                                egui::TextEdit::singleline(&mut edited)
                                    .id(ui.make_persistent_id(("peer-address", id)))
                                    .hint_text(ADDRESS_HINT)
                                    .text_color(theme::ICE)
                                    .background_color(theme::alpha(theme::NAVY, 60))
                                    .margin(egui::Margin::symmetric(12, 9))
                                    .desired_width(width),
                            )
                        })
                        .inner;
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
                    let label = if *role == ToggleRole::RemoveAudioDriver {
                        REMOVE_AUDIO_LABEL
                    } else {
                        label
                    };
                    let response = ui
                        .push_id(("toggle", view.revision, id), |ui| {
                            ui.add_enabled_ui(*enabled, |ui| {
                                theme::toggle(ui, *checked, label, width)
                            })
                            .inner
                        })
                        .inner;
                    reveal_focus(&response);
                    if response.clicked() && *enabled && ui.clip_rect().contains_rect(response.rect)
                    {
                        intents.push(WizardIntent::SetToggle {
                            field: *id,
                            checked: !*checked,
                        });
                    }
                }
            }
            ui.add_space(2.0);
        }
    }

    fn layout(&mut self, ui: &mut egui::Ui, view: &WizardView, intents: &mut Vec<WizardIntent>) {
        let Some(layout) = &view.layout else {
            ui.label(RichText::new("Waiting for the screens' positions…").color(theme::QUIET));
            return;
        };
        self.layout.follow(&layout.confirmed);
        // The canvas receives finite bounds even inside a vertical scroll area.
        let action = ui
            .push_id(("layout", view.revision), |ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), 340.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        // The kit's first row is the Apply/Revert toolbar. Prevent keyboard
                        // activation before calling it: Revert mutates its local editor, so
                        // rejecting a returned action is too late.
                        let toolbar_height = ui.spacing().interact_size.y.max(
                            ui.text_style_height(&TextStyle::Button)
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
                                let (withheld, remaining) = std::mem::take(&mut input.events)
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
                        // Footer activation remains available; the suppression applies only
                        // while the clipped kit toolbar is handling input.
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
    }
}

const ADDRESS_HINT: &str = "For example 192.168.1.20:47811";

/// The message as one text block. A line that is an exact command (the argv a consent shows)
/// is set apart in a code style, so it reads as what will run.
fn message_job(ui: &egui::Ui, message: &str) -> egui::text::LayoutJob {
    let body = egui::TextFormat {
        font_id: TextStyle::Body.resolve(ui.style()),
        color: theme::alpha(theme::ICE, 235),
        line_height: Some(21.0),
        ..Default::default()
    };
    let code = egui::TextFormat {
        font_id: FontId::monospace(13.5),
        color: theme::PEER_ICE,
        background: theme::alpha(theme::NAVY, 85),
        line_height: Some(21.0),
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    for line in message.split_inclusive('\n') {
        let text = line.trim_end_matches('\n');
        let command = text.starts_with("pkexec ") || text.starts_with("/usr/");
        job.append(text, 0.0, if command { code.clone() } else { body.clone() });
        if text.len() < line.len() {
            job.append("\n", 0.0, body.clone());
        }
    }
    job
}

/// The card's header: where you are, the question, and which computer this is.
fn header(ui: &mut egui::Ui, view: &WizardView) {
    let eyebrow = match view.screen {
        ScreenId::Welcome => Some("WELCOME".to_owned()),
        ScreenId::RepairRemove => Some("REMOVE OR REPAIR".to_owned()),
        _ => view.progress.current.map(|group| {
            let step = PROGRESS_GROUPS
                .iter()
                .position(|g| *g == group)
                .map_or(1, |index| index + 1);
            format!(
                "STEP {step} OF {} · {}",
                PROGRESS_GROUPS.len(),
                progress_label(group).to_uppercase()
            )
        }),
    };
    ui.horizontal(|ui| {
        if let Some(eyebrow) = &eyebrow {
            theme::section(ui, eyebrow);
        }
        if view.demo {
            ui.add_space(8.0);
            ui.label(
                RichText::new(DEMO_LABEL)
                    .size(12.5)
                    .color(theme::WARNING)
                    .strong(),
            );
        }
    });
    ui.add_space(2.0);
    ui.label(RichText::new(&view.title).heading().color(theme::ICE));
    if view.machine.is_some() || view.peer.is_some() {
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if let Some(machine) = &view.machine {
                let text = if machine.starts_with("This ") {
                    machine.clone()
                } else {
                    format!("This computer: {machine}")
                };
                chip(ui, &text, theme::QUIET);
            }
            if let Some(peer) = &view.peer {
                chip(ui, &format!("Paired with {peer}"), theme::GLACIER);
            }
        });
    }
}

fn chip(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(theme::alpha(color, 20))
        .stroke((1.0, theme::alpha(color, 60)))
        .corner_radius(20)
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.5).color(color));
        });
}

/// The footer, planned before drawing so the body knows how much room is left: Back on the left
/// as a link, the other buttons on the right with the primary one last. Rows that don't fit
/// move down whole.
struct FooterPlan<'a> {
    back: Option<&'a ButtonView>,
    rows: Vec<Vec<(&'a ButtonView, f32)>>,
    width: f32,
    back_width: f32,
    height: f32,
}

impl<'a> FooterPlan<'a> {
    fn new(ui: &egui::Ui, view: &'a WizardView, width: f32) -> Self {
        let spacing = ui.spacing().item_spacing.x;
        let padding = ui.spacing().button_padding.x;
        let font = TextStyle::Button.resolve(ui.style());
        let text_width = |label: &str| {
            ui.painter()
                .layout_no_wrap(label.to_owned(), font.clone(), theme::ICE)
                .size()
                .x
        };
        let back = view
            .buttons
            .iter()
            .find(|button| button.role == ButtonRole::Back);
        let back_width = back.map_or(0.0, |button| text_width(&button.label) + 4.0);
        let mut actions: Vec<&ButtonView> = view
            .buttons
            .iter()
            .filter(|button| {
                button.role != ButtonRole::Back
                    && matches!(
                        button.kind,
                        ButtonKind::Primary | ButtonKind::Secondary | ButtonKind::Destructive
                    )
            })
            .collect();
        // The primary action sits last, at the right edge.
        actions.sort_by_key(|button| button.kind == ButtonKind::Primary);
        let mut rows: Vec<Vec<(&ButtonView, f32)>> = vec![Vec::new()];
        let mut used = if back.is_some() {
            back_width + spacing * 2.0
        } else {
            0.0
        };
        for button in actions {
            let button_width = text_width(&button.label) + 2.0 * padding;
            let occupied = rows.last().map_or(0, Vec::len);
            if occupied > 0 && used + spacing + button_width > width {
                rows.push(Vec::new());
                used = 0.0;
            }
            let fresh = rows.last().is_none_or(Vec::is_empty);
            used += if fresh {
                button_width
            } else {
                button_width + spacing
            };
            if let Some(row) = rows.last_mut() {
                row.push((button, button_width));
            }
        }
        rows.retain(|row| !row.is_empty());
        let row_height =
            ui.spacing().interact_size.y.max(
                ui.text_style_height(&TextStyle::Button) + 2.0 * ui.spacing().button_padding.y,
            );
        let lines = rows.len().max(usize::from(back.is_some()));
        let height = if lines == 0 {
            0.0
        } else {
            lines as f32 * row_height + (lines - 1) as f32 * ui.spacing().item_spacing.y
        };
        Self {
            back,
            rows,
            width,
            back_width,
            height,
        }
    }

    fn is_empty(&self) -> bool {
        self.back.is_none() && self.rows.is_empty()
    }

    fn show(&self, ui: &mut egui::Ui, view: &WizardView, intents: &mut Vec<WizardIntent>) {
        let rect = ui.available_rect_before_wrap();
        ui.painter().line_segment(
            [rect.left_top(), rect.right_top()],
            (1.0, theme::alpha(theme::GLACIER, 30)),
        );
        ui.add_space(10.0);
        ui.push_id(("wizard-actions", view.revision), |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            let spacing = ui.spacing().item_spacing.x;
            let lines = self.rows.len().max(usize::from(self.back.is_some()));
            for line in 0..lines {
                let row = self.rows.get(line).map_or(&[][..], Vec::as_slice);
                ui.horizontal(|ui| {
                    let mut used = 0.0;
                    let mut items = 0;
                    if line == 0
                        && let Some(back) = self.back
                    {
                        let enabled = back.enabled;
                        let response = ui
                            .push_id(back.id, |ui| {
                                ui.add_enabled_ui(enabled, |ui| theme::link(ui, &back.label))
                                    .inner
                            })
                            .inner;
                        if response.clicked() && enabled {
                            intents.push(WizardIntent::Button(back.id));
                        }
                        used += self.back_width;
                        items += 1;
                    }
                    let row_width: f32 = row.iter().map(|(_, width)| *width).sum::<f32>()
                        + spacing * row.len().saturating_sub(1) as f32;
                    if !row.is_empty() {
                        let gaps = if items > 0 { 2.0 } else { 1.0 } * spacing;
                        ui.add_space((self.width - used - row_width - gaps).max(0.0));
                    }
                    for (button, _) in row {
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
                                    _ => theme::secondary(ui, &button.label),
                                })
                                .inner
                            })
                            .inner;
                        if response.clicked() && enabled {
                            intents.push(WizardIntent::Button(button.id));
                        }
                    }
                });
            }
        });
    }
}

/// Answers to pick from: numbers side by side, statements and computers one under another.
fn choices(ui: &mut egui::Ui, view: &WizardView, intents: &mut Vec<WizardIntent>) {
    let choices: Vec<&ButtonView> = view
        .buttons
        .iter()
        .filter(|button| button.kind == ButtonKind::Choice)
        .collect();
    if choices.is_empty() {
        return;
    }
    ui.add_space(2.0);
    let width = ui.available_width();
    let spacing = ui.spacing().item_spacing.x;
    let short = choices
        .iter()
        .all(|button| button.label.chars().count() <= 12);
    let mut pick = |ui: &mut egui::Ui, button: &ButtonView, width: f32, large: bool| {
        let response = ui
            .push_id(("choice", view.revision, button.id), |ui| {
                ui.add_enabled_ui(button.enabled, |ui| {
                    theme::choice(ui, &button.label, width, large)
                })
                .inner
            })
            .inner;
        reveal_focus(&response);
        if response.clicked() && button.enabled && ui.clip_rect().contains_rect(response.rect) {
            intents.push(WizardIntent::Button(button.id));
        }
    };
    if short {
        let count = choices.len() as f32;
        let tile = ((width - spacing * (count - 1.0)) / count).min(200.0);
        ui.horizontal(|ui| {
            for button in &choices {
                pick(ui, button, tile, true);
            }
        });
    } else {
        for button in &choices {
            pick(ui, button, width, false);
        }
    }
    ui.add_space(2.0);
}

/// Quiet secondary actions under the content, with their heading.
fn links(ui: &mut egui::Ui, view: &WizardView, intents: &mut Vec<WizardIntent>) {
    let links: Vec<&ButtonView> = view
        .buttons
        .iter()
        .filter(|button| button.kind == ButtonKind::Link && button.role != ButtonRole::Back)
        .collect();
    if links.is_empty() {
        return;
    }
    ui.add_space(4.0);
    if let Some(caption) = &view.link_caption {
        ui.label(RichText::new(caption).size(13.0).color(theme::QUIET));
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 18.0;
        for button in links {
            let response = ui
                .push_id(("link", button.id), |ui| {
                    ui.add_enabled_ui(button.enabled, |ui| theme::link(ui, &button.label))
                        .inner
                })
                .inner;
            reveal_focus(&response);
            if response.clicked() && button.enabled && ui.clip_rect().contains_rect(response.rect) {
                intents.push(WizardIntent::Button(button.id));
            }
        }
    });
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

fn mark_of(state: RowState) -> StepMark {
    match state {
        RowState::Unchecked => StepMark::Pending,
        RowState::Working => StepMark::Active,
        RowState::NeedsAction => StepMark::Current,
        RowState::Waiting => StepMark::Waiting,
        RowState::Verified => StepMark::Done,
        RowState::Failed => StepMark::Problem,
        RowState::Unsupported => StepMark::Blocked,
        RowState::Note => StepMark::Info,
    }
}

/// What assistive technology reads for a row's mark; the screen itself shows only the mark.
fn state_words(state: RowState) -> &'static str {
    match state {
        RowState::Unchecked => "not started",
        RowState::Working => "in progress",
        RowState::NeedsAction => "your answer is needed",
        RowState::Waiting => "waiting",
        RowState::Verified => "done",
        RowState::Failed => "stopped",
        RowState::Unsupported => "not available here",
        RowState::Note => "note",
    }
}

/// Done and not-started steps read by their label alone; the rest say what is happening.
fn shows_detail(row: &RowView) -> bool {
    !row.detail.trim().is_empty() && !matches!(row.state, RowState::Unchecked | RowState::Verified)
}

/// The step list, in one quiet panel. Checklist entries are drawn under the step before them.
fn rows(ui: &mut egui::Ui, rows: &[RowView], phase: Option<f32>) {
    egui::Frame::new()
        .fill(theme::alpha(theme::NAVY, 55))
        .stroke((1.0, theme::alpha(theme::GLACIER, 26)))
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            let mut at = 0;
            while at < rows.len() {
                let (row, checks_from) = if rows[at].is_check() {
                    (None, at)
                } else {
                    (Some(&rows[at]), at + 1)
                };
                let checks_to = rows[checks_from..]
                    .iter()
                    .position(|row| !row.is_check())
                    .map_or(rows.len(), |n| checks_from + n);
                let checks = &rows[checks_from..checks_to];
                match row {
                    Some(row) => step_row(ui, row, checks, phase),
                    None => checklist(ui, checks, false),
                }
                at = checks_to;
            }
        });
}

fn step_row(ui: &mut egui::Ui, row: &RowView, checks: &[RowView], phase: Option<f32>) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Label,
                true,
                format!("{}: {}", row.label, state_words(row.state)),
            )
        });
        theme::step_mark(
            ui.painter(),
            rect.shrink(1.0),
            mark_of(row.state),
            if row.state == RowState::Working {
                phase
            } else {
                None
            },
        );
        ui.vertical(|ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            let color = match row.state {
                RowState::Unchecked => theme::alpha(theme::ICE, 150),
                _ => theme::ICE,
            };
            ui.label(RichText::new(&row.label).color(color));
            if shows_detail(row) {
                let color = match row.state {
                    RowState::Failed | RowState::Unsupported => theme::WARNING,
                    _ => theme::QUIET,
                };
                ui.label(RichText::new(&row.detail).size(13.5).color(color));
            }
            if row.human_confirmed {
                ui.label(
                    RichText::new("Confirmed by you")
                        .size(12.5)
                        .color(theme::QUIET),
                );
            }
            checklist(ui, checks, row.state == RowState::Verified);
        });
    });
}

/// The compact checklist: one line per check, with a small mark and its own wording. Once its
/// step is done and every check passed, it folds into one line. Nothing here animates.
fn checklist(ui: &mut egui::Ui, checks: &[RowView], step_done: bool) {
    if checks.is_empty() {
        return;
    }
    if step_done && checks.iter().all(|check| check.state == RowState::Verified) {
        ui.label(
            RichText::new(match checks.len() {
                1 => "Its check passed.".to_owned(),
                count => format!("All {count} checks passed."),
            })
            .size(13.5)
            .color(theme::QUIET),
        );
        return;
    }
    ui.add_space(2.0);
    for check in checks {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
            theme::step_mark(ui.painter(), rect.shrink(1.0), mark_of(check.state), None);
            let style = ui.style().clone();
            let font = egui::FontSelection::FontId(FontId::proportional(13.5));
            let mut line = egui::text::LayoutJob::default();
            RichText::new(format!("{}  ", check.label))
                .color(theme::alpha(theme::ICE, 225))
                .append_to(&mut line, &style, font.clone(), egui::Align::Center);
            RichText::new(check_wording(check))
                .color(mark_of(check.state).color())
                .append_to(&mut line, &style, font, egui::Align::Center);
            ui.add(egui::Label::new(line).wrap());
        });
    }
}

/// How a checklist entry reads: "Checking…", "Passed", "Failed: `reason`" or
/// "Couldn't confirm: `issue`".
pub(crate) fn check_wording(check: &RowView) -> String {
    let detail = check.detail.trim();
    let with = |head: &str| {
        if detail.is_empty() {
            head.to_owned()
        } else {
            format!("{head}: {detail}")
        }
    };
    match check.state {
        RowState::Working => "Checking…".to_owned(),
        RowState::Verified if detail.is_empty() => "Passed".to_owned(),
        RowState::Verified => format!("Passed ({detail})"),
        RowState::Failed | RowState::Unsupported => with("Failed"),
        RowState::Waiting | RowState::NeedsAction => with("Couldn't confirm"),
        RowState::Note => with("Note"),
        RowState::Unchecked => "Not checked yet".to_owned(),
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
        ProgressGroup::PermissionsNetwork => "Permissions",
        ProgressGroup::Connect => "Connect",
        ProgressGroup::Arrange => "Arrange",
        ProgressGroup::Practice => "Practice",
        ProgressGroup::Ready => "Ready",
    }
}

fn illustrated(view: &WizardView) -> bool {
    matches!(
        view.screen,
        ScreenId::Welcome | ScreenId::Grants | ScreenId::AudioComponent | ScreenId::HidingChoice
    ) || (view.screen == ScreenId::MatchNumbers && view.illustration.sas.is_some())
        // While the network screen asks about the firewall, its command is the content.
        || (view.screen == ScreenId::Network
            && !view.rows.iter().any(|row| row.state == RowState::NeedsAction))
        || (view.screen == ScreenId::Summary && view.summary == SummaryView::WorkspaceReady)
        || (view.screen == ScreenId::Permissions && view.illustration.permission_row.is_some())
        || (view.screen == ScreenId::Practice && view.illustration.practice.is_some())
}

/// Illustrations explain; they never crowd the question. Each has a fixed, modest height.
/// The larger ones give way in short windows, so the question stays in view.
fn illustration_height(view: &WizardView, body_height: f32) -> f32 {
    let height: f32 = match view.screen {
        ScreenId::Welcome => 150.0,
        ScreenId::HidingChoice | ScreenId::MatchNumbers => 124.0,
        ScreenId::Practice => 116.0,
        ScreenId::Summary => 86.0,
        _ => 96.0,
    };
    if height > 100.0 {
        height.min((body_height * 0.3).max(96.0))
    } else {
        height
    }
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
        12.0,
        theme::alpha(theme::NAVY, 110),
        theme::alpha(theme::MIDNIGHT, 170),
    );
    let compact = height <= 120.0;
    let content = bounds.shrink2(if compact {
        egui::vec2(12.0, 10.0)
    } else {
        egui::vec2(20.0, 16.0)
    });
    let note_height = if compact { 22.0 } else { 28.0 };
    let diagram = egui::Rect::from_min_max(
        content.min,
        content.max - egui::vec2(0.0, note_height + 4.0),
    );
    let size = egui::vec2(
        (content.width() * 0.23).min(140.0),
        (diagram.height() * 0.66).min(90.0),
    );
    let centre = diagram.center();
    let left = egui::Rect::from_center_size(centre - egui::vec2(size.x * 0.8 + 22.0, 0.0), size);
    let right = egui::Rect::from_center_size(centre + egui::vec2(size.x * 0.8 + 22.0, 0.0), size);
    let note = egui::Rect::from_min_max(
        egui::pos2(content.left(), content.bottom() - note_height),
        content.right_bottom(),
    );
    match view.screen {
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
                diagram.expand2(egui::vec2(0.0, 6.0)),
                digits,
                46.0,
                theme::ICE,
            );
            caption(
                &painter,
                note,
                "Compare on both computers before you confirm",
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
                        let amplitude = 8.0
                            + (fraction * std::f32::consts::PI).sin().abs()
                                * (8.0 + index as f32 * 3.0);
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
            if verified > 0 {
                for index in 0..verified {
                    let centre = centre
                        + egui::vec2(
                            (index as f32 - (verified - 1) as f32 * 0.5) * 46.0,
                            6.0 * (1.0 - fraction),
                        );
                    let rect = egui::Rect::from_center_size(centre, Vec2::splat(28.0));
                    painter.circle_stroke(centre, 18.0, (1.5, theme::FROST));
                    checkmark(&painter, rect, theme::FROST, 0.5 + 0.5 * fraction);
                }
            }
            caption(&painter, note, "Verified just now", 13.0, theme::FROST);
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
