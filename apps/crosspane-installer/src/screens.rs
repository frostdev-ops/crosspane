//! The wizard shell: one card that holds the current question, with progress beside it (wide
//! windows) or above it (narrow ones).
//!
//! Layout rules, so every screen reads the same way:
//! - One column of a fixed, comfortable width. The card fits its content and its height glides
//!   when the content changes; the header stays where it is. Long content scrolls inside the
//!   card while the header and the footer stay put.
//! - A screen has at most one filled (primary) button, bottom right in the footer. Back and the
//!   quiet alternatives are text buttons in the same footer; answers to pick from are tiles in
//!   the content.
//! - Footer buttons are as wide as their text and never wrap: a row that doesn't fit moves to
//!   another line.
//! - A step reads as a mark plus plain words. Its title names the action; the line under it says
//!   how far it got.
//!
//! Motion follows [`MotionLevel`]: full motion slides pages in the direction of travel, lets
//! rows rise in, turns spinners into checks with a small pop and drifts the backdrop for a few
//! seconds; reduced motion keeps only short fades; off is instant. Frames are requested only
//! while something moves.

use std::collections::HashMap;
use std::time::Duration;

use crosspane_ui_kit::theme::{self, ActionKind, StepMark, space, text};
use crosspane_ui_kit::{art::Art, layout::LayoutWidget};
use eframe::egui::{
    self, Color32, FontId, Id, Key, Pos2, Rect, RichText, Sense, TextStyle, UiBuilder, Vec2,
    emath::easing,
};

use crate::demo::{
    DEMO_LABEL, HIDE_LABEL, MICROPHONE_COPY, MICROPHONE_DETAIL, MIRROR_LABEL, REMOVE_AUDIO_LABEL,
};
use crate::motion::{
    IllustrationMotion, MotionLevel, TRANSITION_MS, illustration_allowed, illustration_duration,
    motion_level, progress,
};
use crate::view::*;

/// From this viewport width on, progress is a rail beside the card.
const RAIL_BREAKPOINT: f32 = 860.0;
const RAIL_WIDTH: f32 = 188.0;
const RAIL_GAP: f32 = 36.0;
/// The content column: wide enough for comfortable lines, never wider.
const CARD_MAX_WIDTH: f32 = 600.0;
/// Inside the card.
const CARD_PAD_X: f32 = 32.0;
const CARD_PAD_TOP: f32 = 28.0;
const CARD_PAD_BOTTOM: f32 = 20.0;
const CARD_RADIUS: f32 = 16.0;
/// Narrow windows: the brand bar above the card.
const BAR_HEIGHT: f32 = 36.0;
/// How far a page slides in full motion.
const SLIDE: f32 = 28.0;
/// Rows rise in on a new page, one after another.
const ROW_MS: u64 = 260;
const ROW_STAGGER_MS: u64 = 35;
const ROW_STAGGERED: usize = 4;
const ROW_RISE: f32 = 8.0;
/// A mark changing state: the old one shrinks away and the new one pops in.
const MARK_MS: u64 = 380;
/// Text that changes on the same page cross-fades.
const TEXT_MS: u64 = 220;
/// The card's height follows its content.
const HEIGHT_MS: u64 = 280;
/// One turn of the in-progress mark.
const SPIN_MS: u64 = 1100;
/// One calm breath of a waiting mark.
const BREATH_MS: u64 = 2400;
/// The backdrop drifts this long after a page appears (and while anything else moves).
const AMBIENT_MS: u64 = 6000;
/// Frame pacing while something moves.
const FRAME_MS: u64 = 16;

/// A value easing from where it was to a new target.
#[derive(Clone, Copy, Debug, Default)]
struct Tween {
    from: f32,
    to: f32,
    start: u64,
    duration: u64,
    set: bool,
}

impl Tween {
    fn value(&self, now: u64) -> f32 {
        if !self.set {
            return self.to;
        }
        let t = easing::cubic_out(progress(now, self.start, self.duration));
        self.from + (self.to - self.from) * t
    }

    fn moving(&self, now: u64) -> bool {
        self.set && progress(now, self.start, self.duration) < 1.0
    }

    /// Ease towards `target`; the first call jumps there.
    fn drive(&mut self, target: f32, now: u64, duration: u64) -> f32 {
        if !self.set || duration == 0 {
            *self = Self {
                from: target,
                to: target,
                start: now,
                duration,
                set: true,
            };
        } else if (target - self.to).abs() > 0.01 {
            let current = self.value(now);
            *self = Self {
                from: current,
                to: target,
                start: now,
                duration,
                set: true,
            };
        }
        self.value(now)
    }
}

/// What the shell remembers about one row, to animate it.
#[derive(Clone, Debug)]
struct RowMotion {
    appear: u64,
    state: RowState,
    previous: Option<RowState>,
    changed: u64,
    detail: String,
    old_detail: String,
    detail_changed: u64,
}

/// A piece of text that cross-fades when it changes.
#[derive(Clone, Debug, Default)]
struct TextMotion {
    current: String,
    old: String,
    changed: u64,
}

impl TextMotion {
    fn update(&mut self, text: &str, now: u64) {
        if self.current != text {
            self.old = std::mem::replace(&mut self.current, text.to_owned());
            self.changed = now;
        }
    }
}

/// The page on screen and the one leaving it.
#[derive(Debug, Default)]
struct Page {
    start: u64,
    /// +1 moving forward, -1 going back.
    direction: f32,
    outgoing: Option<Box<WizardView>>,
}

/// The macOS permission rows (WP-4.33): one ordinary row per permission, each with its own
/// actions drawn inside it instead of in the footer or under the content.
pub(crate) const PERMISSION_ROW_IDS: std::ops::Range<u16> = 960..964;
/// The actions of the permission rows: row `PERMISSION_ROW_IDS.start + n` owns the ten ids from
/// `PERMISSION_BUTTON_IDS.start + 10 * n`.
pub(crate) const PERMISSION_BUTTON_IDS: std::ops::Range<u16> = 2600..2640;

/// The permission row a button belongs to, if it is one of their actions.
pub(crate) fn permission_button_row(id: u16) -> Option<u16> {
    PERMISSION_BUTTON_IDS
        .contains(&id)
        .then(|| PERMISSION_ROW_IDS.start + (id - PERMISSION_BUTTON_IDS.start) / 10)
}

fn inline_button_row(id: u16) -> Option<u16> {
    use crate::live::ids;
    permission_button_row(id).or(match id {
        ids::REOPEN_CONNECT => Some(940),
        ids::REOPEN_ARRANGE => Some(941),
        ids::REOPEN_PRACTICE => Some(942),
        _ => None,
    })
}

#[derive(Default)]
pub struct WizardShell {
    layout: LayoutWidget,
    previous: Option<(ScreenId, u64)>,
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
    settings_rect: Option<Rect>,
    page: Page,
    last_view: Option<Box<WizardView>>,
    rows: HashMap<u16, RowMotion>,
    title: TextMotion,
    message: TextMotion,
    card_height: Tween,
    /// The body's natural height, measured on the last frame.
    body_content: f32,
    rail_marker: Tween,
    rail_fill: Tween,
    ambient_ms: u64,
    ambient_last: Option<u64>,
    /// The earliest repaint this frame asked for.
    wake: Option<u64>,
    /// Drawing the page that is leaving: no side effects, no intents.
    outgoing_pass: bool,
    /// This frame uses the narrow layout (no rail).
    narrow: bool,
    /// The title last changed with a page change (it rises in) rather than on the same page.
    title_with_page: bool,
    /// Something moved on the last frame.
    was_moving: bool,
    /// The card's height is gliding: the body hides its scroll bar meanwhile.
    gliding: bool,
}

impl std::fmt::Debug for WizardShell {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WizardShell")
            .field("previous", &self.previous)
            .field("direction", &self.page.direction)
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

/// Where a screen sits in the setup, to slide a page change in its direction of travel.
fn order(screen: ScreenId) -> u8 {
    match screen {
        ScreenId::Welcome => 0,
        ScreenId::Compatibility => 1,
        ScreenId::InstallPlan => 2,
        ScreenId::Installing => 3,
        ScreenId::Permissions => 4,
        ScreenId::AudioComponent => 5,
        ScreenId::Network => 6,
        ScreenId::HidingChoice => 7,
        ScreenId::Connect => 8,
        ScreenId::MatchNumbers => 9,
        ScreenId::Grants => 10,
        ScreenId::Layout => 11,
        ScreenId::Practice => 12,
        ScreenId::Summary => 13,
        ScreenId::RepairRemove => 14,
    }
}

/// Everything the card needs that was decided before it is drawn.
struct FrameState {
    level: MotionLevel,
    now: u64,
    illustration_fraction: f32,
    /// The turn of in-progress marks, or `None` to draw them still.
    phase: Option<f32>,
    /// The breath of waiting marks, 0–1, or `None` to draw them still.
    breath: Option<f32>,
}

/// Lay out `add` as if in place, but drawn `offset` lower and at `opacity`. The space it takes
/// is where it would be without the offset, so nothing around it moves.
fn shifted<R>(
    ui: &mut egui::Ui,
    offset: Vec2,
    opacity: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    if offset == Vec2::ZERO && opacity >= 1.0 {
        return add(ui);
    }
    let available = ui.available_rect_before_wrap();
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(available.translate(offset))
            .layout(*ui.layout()),
    );
    child.multiply_opacity(opacity);
    let result = add(&mut child);
    ui.advance_cursor_after_rect(child.min_rect().translate(-offset));
    result
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

    fn wake_in(&mut self, ms: u64) {
        self.wake = Some(self.wake.map_or(ms, |current| current.min(ms)));
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        art: &Art,
        now_ms: u64,
    ) -> Vec<WizardAction> {
        let mut intents = Vec::new();
        self.wake = None;
        let level = motion_level(view.motion, view.system_reduced_motion);
        let reduced = !level.moves();
        {
            let style = ui.style_mut();
            style.animation_time = level.hover_seconds();
            style.scroll_animation = if reduced {
                egui::style::ScrollAnimation::none()
            } else {
                egui::style::ScrollAnimation::duration(level.hover_seconds())
            };
            style.text_styles.insert(TextStyle::Body, text::body());
            style
                .text_styles
                .insert(TextStyle::Button, FontId::proportional(14.5));
            style.text_styles.insert(TextStyle::Heading, text::title());
            style.text_styles.insert(TextStyle::Small, text::small());
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
            let leaving = self.previous.and(self.last_view.take());
            self.page = Page {
                start: now_ms,
                direction: match &leaving {
                    Some(old) if order(old.screen) > order(view.screen) => -1.0,
                    _ => 1.0,
                },
                outgoing: leaving.filter(|_| level != MotionLevel::Off),
            };
            self.rows.clear();
            // The title changes with the page: the old one fades as the new one rises in.
            let leaving_title = std::mem::take(&mut self.title.current);
            self.title = TextMotion {
                current: view.title.clone(),
                old: if self.previous.is_some() && level != MotionLevel::Off {
                    leaving_title
                } else {
                    String::new()
                },
                changed: now_ms,
            };
            self.title_with_page = true;
            self.message = TextMotion {
                current: view.message.clone(),
                ..TextMotion::default()
            };
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
        if page_changed || revision_changed || self.last_view.is_none() {
            self.last_view = Some(Box::new(view.clone()));
        }
        self.previous = Some((view.screen, view.revision));
        self.address_focus.clear();
        if self.title.current != view.title {
            self.title_with_page = false;
        }
        self.title.update(&view.title, now_ms);
        self.message.update(&view.message, now_ms);

        let page_t = progress(now_ms, self.page.start, level.duration(TRANSITION_MS));
        if page_t < 1.0 {
            self.wake_in(FRAME_MS);
        } else {
            self.page.outgoing = None;
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
            self.wake_in(FRAME_MS);
        }
        // Work in progress turns its mark and waiting breathes, only in full motion; otherwise
        // marks are drawn still and ask for no repaint.
        let spinning = !reduced && view.rows.iter().any(|row| row.state == RowState::Working);
        let phase = spinning.then(|| (now_ms % SPIN_MS) as f32 / SPIN_MS as f32);
        if spinning {
            self.wake_in(FRAME_MS);
        }
        let breathing = !reduced
            && view
                .rows
                .iter()
                .any(|row| matches!(row.state, RowState::Waiting | RowState::NeedsAction));
        let breath = breathing.then(|| (now_ms % BREATH_MS) as f32 / BREATH_MS as f32);
        if breathing {
            self.wake_in(33);
        }
        if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            match view.escape {
                EscapeMapping::None => {}
                EscapeMapping::Back => intents.push(WizardIntent::Back),
                EscapeMapping::Close => intents.push(WizardIntent::Close),
            }
        }
        let frame = FrameState {
            level,
            now: now_ms,
            illustration_fraction,
            phase,
            breath,
        };
        let viewport = ui.ctx().viewport_rect();
        self.background(ui, art, viewport, &frame);
        let region = ui.available_rect_before_wrap();
        let wide = viewport.width() >= RAIL_BREAKPOINT;
        self.narrow = !wide;
        let pad = if wide { space::XXL } else { space::XL };
        let inner = region.shrink(pad.min(region.width() * 0.04).max(space::S));
        if wide {
            let card_width = (inner.width() - RAIL_WIDTH - RAIL_GAP).min(CARD_MAX_WIDTH);
            let group = RAIL_WIDTH + RAIL_GAP + card_width;
            let left = inner.center().x - group * 0.5;
            let top = self.column_top(inner, frame.now);
            let card = Rect::from_min_size(
                Pos2::new(left + RAIL_WIDTH + RAIL_GAP, top),
                Vec2::new(card_width, inner.bottom() - top),
            );
            let card = self.card(ui, view, &mut intents, &frame, card);
            let rail = Rect::from_min_max(
                Pos2::new(left, top),
                Pos2::new(left + RAIL_WIDTH, card.bottom().max(top + 360.0)),
            );
            self.rail(ui, view, art, &mut intents, rail, &frame);
        } else {
            let width = inner.width().min(CARD_MAX_WIDTH);
            let left = inner.center().x - width * 0.5;
            let top = self.column_top(inner, frame.now);
            let bar = Rect::from_min_size(Pos2::new(left, top), Vec2::new(width, BAR_HEIGHT));
            self.top_bar(ui, view, art, &mut intents, bar);
            let card_top = bar.bottom() + space::L;
            let card = Rect::from_min_max(
                Pos2::new(left, card_top),
                Pos2::new(left + width, inner.bottom()),
            );
            self.card(ui, view, &mut intents, &frame, card);
        }
        self.settings_button(ui, view, &mut intents);
        // Keep the whole region allocated, so the panel never shrinks around the card.
        ui.advance_cursor_after_rect(region);
        let others_moving = self.wake.is_some_and(|ms| ms <= FRAME_MS);
        // One more frame after motion ends, so layout that follows the last moving frame (a
        // scroll bar, a wrapped line) settles before the window goes idle.
        if self.was_moving && !others_moving {
            self.wake_in(FRAME_MS);
        }
        self.was_moving = others_moving;
        self.ambient(now_ms, level, others_moving);
        if let Some(ms) = self.wake {
            ui.ctx().request_repaint_after(Duration::from_millis(ms));
        }
        intents
            .into_iter()
            .map(|intent| WizardAction {
                revision: view.revision,
                intent,
            })
            .collect()
    }

    /// Where the column starts: a fixed share of the spare height, from the window alone, so the
    /// header never moves when a screen's content grows or shrinks.
    fn column_top(&self, inner: Rect, _now: u64) -> f32 {
        let typical = 540.0;
        inner.top() + ((inner.height() - typical) * 0.38).max(0.0)
    }

    /// The backdrop drifts slowly while a page settles in and while anything else moves; then it
    /// rests, and an idle screen asks for no frames.
    fn ambient(&mut self, now: u64, level: MotionLevel, others_moving: bool) {
        let active =
            level.moves() && (now.saturating_sub(self.page.start) < AMBIENT_MS || others_moving);
        if active {
            if let Some(last) = self.ambient_last {
                self.ambient_ms += now.saturating_sub(last).min(100);
            }
            self.ambient_last = Some(now);
            self.wake_in(FRAME_MS * 2);
        } else {
            self.ambient_last = None;
        }
    }

    fn background(&self, ui: &egui::Ui, art: &Art, viewport: Rect, frame: &FrameState) {
        let painter = ui.painter().with_clip_rect(viewport);
        art.background(&painter, viewport);
        // Quiet the artwork: the content is the subject.
        painter.rect_filled(viewport, 0.0, theme::alpha(theme::MIDNIGHT, 120));
        let t = self.ambient_ms as f32 / 1000.0;
        let size = viewport.size();
        let at = |x: f32, y: f32| viewport.min + Vec2::new(x * size.x, y * size.y);
        let reach = size.x.max(size.y);
        let drift = if frame.level == MotionLevel::Off {
            0.0
        } else {
            1.0
        };
        theme::radial_glow(
            &painter,
            at(
                0.20 + 0.05 * (t * 0.21).sin() * drift,
                0.25 + 0.06 * (t * 0.17).cos() * drift,
            ),
            reach * 0.55,
            theme::alpha(theme::FROST, 22),
        );
        theme::radial_glow(
            &painter,
            at(
                0.82 + 0.05 * (t * 0.13 + 1.3).cos() * drift,
                0.78 + 0.05 * (t * 0.19 + 0.4).sin() * drift,
            ),
            reach * 0.5,
            theme::alpha(theme::NAVY, 90),
        );
        theme::radial_glow(
            &painter,
            at(
                0.55 + 0.08 * (t * 0.09 + 2.0).sin() * drift,
                0.05 + 0.04 * (t * 0.23).cos() * drift,
            ),
            reach * 0.32,
            theme::alpha(theme::GLACIER, 12),
        );
    }

    /// Wide windows: brand, the six progress groups and the settings affordance.
    fn rail(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        art: &Art,
        intents: &mut Vec<WizardIntent>,
        rect: Rect,
        frame: &FrameState,
    ) {
        let _ = intents;
        let mut ui = ui.new_child(UiBuilder::new().max_rect(rect));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            art.emblem(ui, Vec2::splat(30.0));
            // The read-only compact wordmark is 689×96 pixels.
            art.wordmark(ui, Vec2::new(689.0 / 96.0 * 15.0, 15.0));
        });
        let painter = ui.painter().clone();
        let first = rect.top() + 84.0;
        let step = 44.0;
        let mark_x = rect.left() + 10.0;
        let current = view
            .progress
            .current
            .and_then(|group| PROGRESS_GROUPS.iter().position(|g| *g == group));
        let done_count = PROGRESS_GROUPS
            .iter()
            .take_while(|group| view.progress.completed.contains(group))
            .count();
        let duration = if frame.level.moves() { 420 } else { 0 };
        let marker = current.map(|index| self.rail_marker.drive(index as f32, frame.now, duration));
        let fill = self.rail_fill.drive(done_count as f32, frame.now, duration);
        if self.rail_marker.moving(frame.now) || self.rail_fill.moving(frame.now) {
            self.wake_in(FRAME_MS);
        }
        // The highlight behind the current step glides to it.
        if let Some(marker) = marker {
            let y = first + marker * step;
            let pill = Rect::from_min_max(
                Pos2::new(rect.left() - 8.0, y - 17.0),
                Pos2::new(rect.right(), y + 17.0),
            );
            painter.rect_filled(pill, 10.0, theme::alpha(theme::FROST, 16));
        }
        // The track between marks, and how far setup has come along it.
        let last = first + (PROGRESS_GROUPS.len() - 1) as f32 * step;
        painter.line_segment(
            [Pos2::new(mark_x, first), Pos2::new(mark_x, last)],
            (1.5, theme::alpha(theme::QUIET, 40)),
        );
        let reached = (first + fill.min((PROGRESS_GROUPS.len() - 1) as f32) * step).min(last);
        if fill > 0.0 {
            painter.line_segment(
                [Pos2::new(mark_x, first), Pos2::new(mark_x, reached)],
                (1.5, theme::alpha(theme::FROST, 170)),
            );
        }
        for (index, group) in PROGRESS_GROUPS.into_iter().enumerate() {
            let is_current = view.progress.current == Some(group);
            let done = view.progress.completed.contains(&group);
            let label = progress_label(group);
            let y = first + index as f32 * step;
            let row = Rect::from_min_max(
                Pos2::new(rect.left(), y - 16.0),
                Pos2::new(rect.right(), y + 16.0),
            );
            let response = ui.interact(row, ui.id().with(("rail", index)), Sense::hover());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Label,
                    true,
                    format!(
                        "{label}: {}",
                        if done {
                            "done"
                        } else if is_current {
                            "current step"
                        } else {
                            "not started"
                        }
                    ),
                )
            });
            let mark_rect = Rect::from_center_size(Pos2::new(mark_x, y), Vec2::splat(18.0));
            // A solid disc under every mark, so the track never shows through.
            painter.circle_filled(mark_rect.center(), 8.0, theme::card_fill());
            let mark = if done {
                StepMark::Done
            } else if is_current {
                StepMark::Current
            } else {
                StepMark::Pending
            };
            theme::step_mark(&painter, mark_rect, mark, None);
            let color = if is_current {
                theme::ICE
            } else if done {
                theme::alpha(theme::ICE, 200)
            } else {
                theme::alpha(theme::QUIET, 160)
            };
            painter.text(
                Pos2::new(rect.left() + 30.0, y),
                egui::Align2::LEFT_CENTER,
                label,
                FontId::proportional(14.5),
                color,
            );
        }
        // Bottom: which computer this is, and the motion settings.
        let bottom = rect.bottom();
        let gear = Rect::from_min_size(
            Pos2::new(rect.right() - 30.0, bottom - 30.0),
            Vec2::splat(30.0),
        );
        self.settings_rect = Some(gear);
        if let Some(machine) = machine_line(view) {
            let galley = painter.layout(
                machine,
                text::caption(),
                theme::alpha(theme::QUIET, 210),
                rect.width() - 40.0,
            );
            painter.galley(
                Pos2::new(rect.left(), gear.center().y - galley.size().y * 0.5),
                galley,
                theme::QUIET,
            );
        }
    }

    /// Narrow windows: brand, which computer this is, settings, and a segmented progress strip.
    fn top_bar(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        art: &Art,
        intents: &mut Vec<WizardIntent>,
        rect: Rect,
    ) {
        let _ = intents;
        let mut bar = ui.new_child(UiBuilder::new().max_rect(rect));
        bar.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            art.emblem(ui, Vec2::splat(26.0));
            art.wordmark(ui, Vec2::new(689.0 / 96.0 * 13.0, 13.0));
        });
        let gear = Rect::from_min_size(
            Pos2::new(rect.right() - 30.0, rect.top() - 2.0),
            Vec2::splat(30.0),
        );
        self.settings_rect = Some(gear);
        if let Some(machine) =
            machine_line(view).and_then(|line| line.lines().next().map(str::to_owned))
        {
            bar.painter().text(
                Pos2::new(gear.left() - 8.0, gear.center().y),
                egui::Align2::RIGHT_CENTER,
                machine,
                text::caption(),
                theme::alpha(theme::QUIET, 210),
            );
        }
        let strip = Rect::from_min_size(
            Pos2::new(rect.left(), rect.bottom() - 4.0),
            Vec2::new(rect.width(), 4.0),
        );
        let done: Vec<bool> = PROGRESS_GROUPS
            .iter()
            .map(|group| view.progress.completed.contains(group))
            .collect();
        let current = view
            .progress
            .current
            .and_then(|group| PROGRESS_GROUPS.iter().position(|g| *g == group));
        let response = ui.interact(strip, ui.id().with("progress-strip"), Sense::hover());
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

    /// The gear, drawn last so it comes last in keyboard order, and its small popover.
    fn settings_button(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        intents: &mut Vec<WizardIntent>,
    ) {
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
        let t = ui.ctx().animate_bool_with_time(
            response.id.with("lit"),
            lit,
            ui.style().animation_time,
        );
        if t > 0.0 {
            ui.painter()
                .rect_filled(rect, 8.0, theme::alpha(theme::GLACIER, (22.0 * t) as u8));
        }
        theme::gear(
            ui.painter(),
            rect.shrink(8.0),
            theme::mix(theme::alpha(theme::QUIET, 190), theme::GLACIER, t),
        );
        if response.has_focus() {
            ui.painter()
                .rect_stroke(rect, 8.0, (1.5, theme::GLACIER), egui::StrokeKind::Inside);
        }
        if response.clicked() {
            self.settings_open = !self.settings_open;
        }
        let response = response.on_hover_text("Motion settings");
        if !self.settings_open {
            return;
        }
        let below = rect.bottom() + 220.0 < ui.ctx().viewport_rect().bottom();
        let anchor = if below {
            (
                egui::Align2::RIGHT_TOP,
                rect.right_bottom() + Vec2::new(0.0, 6.0),
            )
        } else {
            (
                egui::Align2::RIGHT_BOTTOM,
                rect.right_top() - Vec2::new(0.0, 6.0),
            )
        };
        let area = egui::Area::new(ui.make_persistent_id("motion-settings-panel"))
            .order(egui::Order::Foreground)
            .pivot(anchor.0)
            .fixed_pos(anchor.1)
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(theme::card_fill())
                    .stroke((1.0, theme::card_stroke()))
                    .corner_radius(12)
                    .inner_margin(egui::Margin::symmetric(14, 12))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 6],
                        blur: 20,
                        spread: 0,
                        color: Color32::from_black_alpha(90),
                    })
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("Motion")
                                .font(text::caption())
                                .color(theme::QUIET),
                        );
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            for (preference, label) in [
                                (MotionPreference::Auto, "Auto"),
                                (MotionPreference::Full, "Full"),
                                (MotionPreference::Reduced, "Reduced"),
                                (MotionPreference::Off, "Off"),
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
            });
        // A click anywhere else closes it.
        if ui.input(|input| input.pointer.any_click())
            && !response.hovered()
            && !area.response.hovered()
            && !area.response.contains_pointer()
        {
            self.settings_open = false;
        }
    }

    /// The card: header, body and footer. Its height follows its content, capped by `bounds`;
    /// it returns the rectangle it drew.
    fn card(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        intents: &mut Vec<WizardIntent>,
        frame: &FrameState,
        bounds: Rect,
    ) -> Rect {
        let inner_width = bounds.width() - 2.0 * CARD_PAD_X;
        let content_left = bounds.left() + CARD_PAD_X;
        let plan = FooterPlan::new(ui, view, inner_width);
        let footer_height = if plan.is_empty() {
            0.0
        } else {
            plan.height + space::L + 1.0 + space::L
        };
        // The header is laid out first, to know where the body starts.
        let header = self.header_layout(ui, view, inner_width);
        let fixed = CARD_PAD_TOP + header.height + space::L + footer_height + CARD_PAD_BOTTOM;
        let wanted = (fixed + self.body_content)
            .min(bounds.height())
            .max(fixed + 48.0);
        let duration = if frame.level.moves() { HEIGHT_MS } else { 0 };
        let height = self.card_height.drive(wanted, frame.now, duration);
        self.gliding = self.card_height.moving(frame.now);
        if self.gliding {
            self.wake_in(FRAME_MS);
        }
        let height = height.min(bounds.height());
        let card = Rect::from_min_size(bounds.min, Vec2::new(bounds.width(), height));
        let painter = ui.painter().clone();
        painter.add(
            egui::epaint::Shadow {
                offset: [0, 12],
                blur: 40,
                spread: 0,
                color: Color32::from_black_alpha(110),
            }
            .as_shape(card, CARD_RADIUS),
        );
        painter.rect(
            card,
            CARD_RADIUS,
            theme::card_fill(),
            (1.0, theme::card_stroke()),
            egui::StrokeKind::Inside,
        );
        // A faint sheen along the top edge.
        painter.line_segment(
            [
                card.left_top() + Vec2::new(CARD_RADIUS, 0.5),
                card.right_top() + Vec2::new(-CARD_RADIUS, 0.5),
            ],
            (1.0, theme::alpha(theme::GLACIER, 40)),
        );
        if view.demo {
            // Wide: above the card's corner. Narrow: on the header's first line.
            let corner = if self.narrow {
                Pos2::new(card.right() - CARD_PAD_X, card.top() + CARD_PAD_TOP + 14.0)
            } else {
                Pos2::new(card.right(), card.top() - space::S)
            };
            demo_badge(&painter, corner);
        }
        let header_rect = Rect::from_min_size(
            Pos2::new(content_left, card.top() + CARD_PAD_TOP),
            Vec2::new(inner_width, header.height),
        );
        self.header(ui, view, header, header_rect, frame);
        let body_top = header_rect.bottom() + space::L;
        let footer_top = card.bottom() - CARD_PAD_BOTTOM - plan.height;
        let body_bottom = if plan.is_empty() {
            card.bottom() - CARD_PAD_BOTTOM
        } else {
            footer_top - space::L - 1.0 - space::L
        };
        let body = Rect::from_min_max(
            Pos2::new(content_left, body_top),
            Pos2::new(content_left + inner_width, body_bottom.max(body_top + 24.0)),
        );
        let room = bounds.height() - fixed;
        let natural = self.body_region(ui, view, intents, frame, body, room);
        if (natural - self.body_content).abs() > 0.5 {
            self.body_content = natural;
            self.wake_in(0);
        }
        if !plan.is_empty() {
            let divider = body.bottom() + space::L;
            painter.line_segment(
                [
                    Pos2::new(card.left() + 1.0, divider),
                    Pos2::new(card.right() - 1.0, divider),
                ],
                (1.0, theme::alpha(theme::GLACIER, 22)),
            );
            let footer = Rect::from_min_size(
                Pos2::new(content_left, divider + 1.0 + space::L),
                Vec2::new(inner_width, plan.height),
            );
            let mut child = ui.new_child(UiBuilder::new().max_rect(footer));
            plan.show(&mut child, view, intents);
        }
        card
    }

    /// The body: the page on screen, and in motion the page leaving it. Returns the natural
    /// height of the page on screen.
    fn body_region(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        intents: &mut Vec<WizardIntent>,
        frame: &FrameState,
        body: Rect,
        room: f32,
    ) -> f32 {
        let duration = frame.level.duration(TRANSITION_MS);
        let t = progress(frame.now, self.page.start, duration);
        let slide = if frame.level.moves() { SLIDE } else { 0.0 };
        let direction = self.page.direction;
        if let Some(old) = self.page.outgoing.take() {
            // The leaving page goes in the first half, sliding the other way.
            let out = (t / 0.4).min(1.0);
            if out < 1.0 && old.screen != ScreenId::Layout {
                let offset = Vec2::new(-direction * slide * easing::cubic_in(out), 0.0);
                let mut child = ui.new_child(UiBuilder::new().max_rect(body.translate(offset)));
                child.set_clip_rect(body.intersect(ui.clip_rect()));
                child.multiply_opacity(1.0 - easing::cubic_out(out));
                child.disable();
                child.visuals_mut().disabled_alpha = 1.0;
                self.outgoing_pass = true;
                let mut ignored = Vec::new();
                child.push_id("outgoing-page", |ui| {
                    self.content(ui, &old, &mut ignored, frame, room);
                });
                self.outgoing_pass = false;
            }
            self.page.outgoing = Some(old);
        }
        let eased = easing::cubic_out(((t - 0.3) / 0.7).clamp(0.0, 1.0));
        let offset = Vec2::new(direction * slide * (1.0 - eased), 0.0);
        let mut child = ui.new_child(UiBuilder::new().max_rect(body.translate(offset)));
        child.set_clip_rect(body.expand2(Vec2::new(0.0, 2.0)).intersect(ui.clip_rect()));
        child.multiply_opacity(if duration == 0 { 1.0 } else { eased });
        self.scroll_body(&mut child, view, intents, frame, body.height(), room)
    }

    /// The scrolling body. Returns the content's natural height.
    fn scroll_body(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        intents: &mut Vec<WizardIntent>,
        frame: &FrameState,
        body_height: f32,
        room: f32,
    ) -> f32 {
        let scroll_focus =
            ui.make_persistent_id(("wizard-scroll-focus", format!("{:?}", view.screen)));
        ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
        ui.spacing_mut().scroll.foreground_color = true;
        ui.spacing_mut().scroll.bar_width = 6.0;
        let scroll = egui::ScrollArea::vertical()
            .id_salt(("wizard-body", format!("{:?}", view.screen)))
            // While the card's height glides, content briefly overflows: no flickering bar.
            .scroll_bar_visibility(if self.gliding {
                egui::scroll_area::ScrollBarVisibility::AlwaysHidden
            } else {
                egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded
            })
            // Commit focus-reveal offsets in this pass, before the next paint.
            // A zero-duration target still takes an extra pass when animated.
            .animated(false)
            .max_height(body_height)
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
                let top = ui.cursor().min.y;
                self.content(ui, view, intents, frame, room);
                ui.min_rect().bottom() - top
            });
        // Soft edges where content continues out of sight.
        let viewport = scroll.inner_rect;
        let offset = scroll.state.offset.y;
        let overflow = scroll.content_size.y - viewport.height();
        let painter = ui.painter().with_clip_rect(viewport);
        let fade = 18.0;
        if offset > 1.0 {
            theme::gradient(
                &painter,
                Rect::from_min_size(viewport.min, Vec2::new(viewport.width(), fade)),
                0.0,
                theme::card_fill(),
                theme::alpha(theme::card_fill(), 0),
            );
        }
        if overflow - offset > 1.0 {
            theme::gradient(
                &painter,
                Rect::from_min_max(
                    Pos2::new(viewport.left(), viewport.bottom() - fade),
                    viewport.right_bottom(),
                ),
                0.0,
                theme::alpha(theme::card_fill(), 0),
                theme::card_fill(),
            );
        }
        if ui.memory(|memory| memory.has_focus(scroll_focus)) {
            ui.painter().rect_stroke(
                scroll.inner_rect,
                4.0,
                (1.0, theme::FROST),
                egui::StrokeKind::Inside,
            );
        }
        scroll.inner
    }

    /// Everything inside the body, top to bottom.
    fn content(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        intents: &mut Vec<WizardIntent>,
        frame: &FrameState,
        body_height: f32,
    ) {
        ui.spacing_mut().item_spacing.y = 8.0;
        if !view.message.is_empty() {
            self.message_text(ui, view, frame);
            ui.add_space(space::S);
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
            ui.add_space(space::XS);
        }
        if view.screen == ScreenId::AudioComponent
            && !view
                .rows
                .iter()
                .any(|row| row.detail.contains(MICROPHONE_COPY))
        {
            ui.label(
                RichText::new(MICROPHONE_DETAIL)
                    .font(text::caption())
                    .color(theme::QUIET),
            );
            ui.add_space(space::XS);
        }
        if illustrated(view) && !gives_way(view.screen, body_height) {
            illustration(
                ui,
                view,
                illustration_height(view, body_height),
                frame.illustration_fraction,
            );
            ui.add_space(space::S);
        }
        if !view.rows.is_empty() {
            self.steps(ui, view, frame, intents);
        }
        self.fields(ui, view, intents);
        if view.screen == ScreenId::Layout && !self.outgoing_pass {
            self.layout(ui, view, intents);
        }
        choices(ui, view, intents);
        links(ui, view, intents);
    }

    /// The screen's message. A change on the same page cross-fades.
    fn message_text(&mut self, ui: &mut egui::Ui, view: &WizardView, frame: &FrameState) {
        let t = if self.outgoing_pass || self.message.old.is_empty() {
            1.0
        } else {
            progress(
                frame.now,
                self.message.changed,
                frame.level.duration(TEXT_MS),
            )
        };
        if t < 1.0 {
            self.wake_in(FRAME_MS);
        }
        let response = ui
            .scope(|ui| {
                ui.multiply_opacity(t);
                ui.add(egui::Label::new(message_job(ui, &view.message)).wrap())
            })
            .inner;
        if t < 1.0 {
            let old = message_job(ui, &self.message.old);
            let galley = ui.painter().layout_job(egui::text::LayoutJob {
                wrap: egui::text::TextWrapping::wrap_at_width(response.rect.width()),
                ..old
            });
            let mut painter = ui.painter().clone();
            painter.multiply_opacity(1.0 - t);
            painter.galley(response.rect.min, galley, theme::ICE);
        }
    }

    /// The page's steps as one list. Checklist entries are drawn under the step before them.
    fn steps(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        frame: &FrameState,
        intents: &mut Vec<WizardIntent>,
    ) {
        let rows = &view.rows;
        let mut marks: Vec<(Pos2, RowState, f32)> = Vec::new();
        let mut at = 0;
        let mut index = 0;
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
                Some(row) => {
                    let motion = self.row_motion(row, index, frame);
                    let rise = if frame.level.moves() {
                        ROW_RISE * (1.0 - motion.appear)
                    } else {
                        0.0
                    };
                    let actions: Vec<&ButtonView> = view
                        .buttons
                        .iter()
                        .filter(|button| inline_button_row(button.id) == Some(row.id))
                        .collect();
                    let mark = shifted(ui, Vec2::new(0.0, rise), motion.appear, |ui| {
                        let mark = self.step_row(ui, row, checks, &motion, frame);
                        row_actions(ui, view, &actions, intents);
                        mark
                    });
                    marks.push((mark, row.state, motion.appear));
                    index += 1;
                }
                None => {
                    ui.horizontal_top(|ui| {
                        ui.add_space(34.0);
                        ui.vertical(|ui| self.checklist(ui, 0, checks, false));
                    });
                }
            }
            ui.add_space(space::S);
            at = checks_to;
        }
        // The thread between consecutive steps: lit where a step is done.
        let painter = ui.painter();
        for pair in marks.windows(2) {
            let (a, state, appear_a) = pair[0];
            let (b, _, appear_b) = pair[1];
            let from = a + Vec2::new(0.0, 14.0);
            let to = b - Vec2::new(0.0, 14.0);
            if to.y - from.y < 4.0 {
                continue;
            }
            let opacity = appear_a.min(appear_b);
            let color = if state == RowState::Verified {
                theme::alpha(theme::FROST, (130.0 * opacity) as u8)
            } else {
                theme::alpha(theme::QUIET, (45.0 * opacity) as u8)
            };
            painter.line_segment([from, to], (1.5, color));
        }
        ui.add_space(space::XS);
    }

    fn row_motion(&mut self, row: &RowView, index: usize, frame: &FrameState) -> DrawnRow {
        let now = frame.now;
        if self.outgoing_pass {
            return DrawnRow {
                appear: 1.0,
                pop: 1.0,
                previous: None,
                detail: 1.0,
                old_detail: String::new(),
            };
        }
        let entry_delay = if frame.level.moves() {
            ROW_STAGGER_MS * index.min(ROW_STAGGERED) as u64
        } else {
            0
        };
        let page_start = self.page.start;
        let fresh_page = now.saturating_sub(page_start) < 50;
        let motion = self.rows.entry(row.id).or_insert_with(|| RowMotion {
            appear: if fresh_page {
                page_start + entry_delay
            } else {
                now
            },
            state: row.state,
            previous: None,
            changed: 0,
            detail: row_detail(row),
            old_detail: String::new(),
            detail_changed: 0,
        });
        if motion.state != row.state {
            motion.previous = Some(motion.state);
            motion.state = row.state;
            motion.changed = now;
        }
        let detail_text = row_detail(row);
        if motion.detail != detail_text {
            motion.old_detail = std::mem::replace(&mut motion.detail, detail_text);
            motion.detail_changed = now;
        }
        let appear = easing::cubic_out(progress(now, motion.appear, frame.level.duration(ROW_MS)));
        let pop = progress(now, motion.changed, frame.level.duration(MARK_MS));
        let detail = progress(now, motion.detail_changed, frame.level.duration(TEXT_MS));
        let previous = motion.previous.filter(|_| pop < 1.0);
        let old_detail = if detail < 1.0 {
            motion.old_detail.clone()
        } else {
            String::new()
        };
        if appear < 1.0 || pop < 1.0 || detail < 1.0 {
            self.wake_in(FRAME_MS);
        }
        DrawnRow {
            appear,
            pop,
            previous,
            detail,
            old_detail,
        }
    }

    /// One step: its mark, its title, and the line that says how far it got.
    fn step_row(
        &mut self,
        ui: &mut egui::Ui,
        row: &RowView,
        checks: &[RowView],
        motion: &DrawnRow,
        frame: &FrameState,
    ) -> Pos2 {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Label,
                    true,
                    format!("{}: {}", row.label, state_words(row.state)),
                )
            });
            draw_mark(ui.painter(), rect.shrink(1.0), row.state, motion, frame);
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 2.0;
                let color = match row.state {
                    RowState::Unchecked => theme::alpha(theme::ICE, 140),
                    _ => theme::ICE,
                };
                ui.add_space(1.0);
                ui.label(RichText::new(&row.label).font(text::body()).color(color));
                if shows_detail(row) && !(row.state == RowState::Verified && !checks.is_empty()) {
                    let color = match row.state {
                        RowState::Failed | RowState::Unsupported => theme::WARNING,
                        _ => theme::QUIET,
                    };
                    let galley = ui.painter().layout(
                        row_detail(row),
                        text::caption(),
                        color,
                        ui.available_width(),
                    );
                    let height = ui.ctx().animate_value_with_time(
                        ui.id().with(("status-height", row.id)),
                        galley.size().y,
                        if frame.level.moves() {
                            HEIGHT_MS as f32 / 1000.0
                        } else {
                            0.0
                        },
                    );
                    let (rect, response) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), height),
                        Sense::hover(),
                    );
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, row_detail(row))
                    });
                    let mut current = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                    current.multiply_opacity(motion.detail);
                    current.galley(rect.min, galley, color);
                    if !motion.old_detail.is_empty() {
                        let mut old = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                        old.multiply_opacity(1.0 - motion.detail);
                        let galley = old.layout(
                            motion.old_detail.clone(),
                            text::caption(),
                            color,
                            rect.width(),
                        );
                        old.galley(rect.min, galley, color);
                    }
                }
                if row.human_confirmed {
                    ui.label(
                        RichText::new("Confirmed by you")
                            .font(text::caption())
                            .color(theme::QUIET),
                    );
                }
                self.checklist(ui, row.id, checks, row.state == RowState::Verified);
            });
            rect.center()
        })
        .inner
    }

    /// The checks under a step. Passed checks and notes fold into one line with a disclosure;
    /// only what needs attention, or is still being checked, is listed.
    fn checklist(&mut self, ui: &mut egui::Ui, step: u16, checks: &[RowView], step_done: bool) {
        if checks.is_empty() {
            return;
        }
        let quiet = |check: &&RowView| matches!(check.state, RowState::Verified | RowState::Note);
        let open: Vec<&RowView> = checks.iter().filter(|check| !quiet(check)).collect();
        let settled: Vec<&RowView> = checks.iter().filter(quiet).collect();
        let all_working = checks.iter().all(|check| check.state == RowState::Working);
        if all_working {
            ui.label(
                RichText::new(format!(
                    "Checking {} item{}…",
                    checks.len(),
                    if checks.len() == 1 { "" } else { "s" }
                ))
                .font(text::caption())
                .color(theme::QUIET),
            );
            return;
        }
        ui.add_space(2.0);
        for check in &open {
            check_line(ui, check);
        }
        if settled.is_empty() {
            return;
        }
        let passed = settled
            .iter()
            .filter(|check| check.state == RowState::Verified)
            .count();
        let summary = match (open.is_empty(), passed) {
            (_, 0) => "Checks complete".to_owned(),
            (true, 1) if step_done || settled.len() == 1 => "Its check passed".to_owned(),
            (true, count) if count == settled.len() => format!("All {count} checks passed"),
            (true, count) => format!("{count} checks passed"),
            (false, 1) => "1 other check passed".to_owned(),
            (false, count) => format!("{count} other checks passed"),
        };
        let id = ui.make_persistent_id(("checks", step));
        let mut state =
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.label(
                RichText::new(summary)
                    .font(text::caption())
                    .color(theme::QUIET),
            );
            if !self.outgoing_pass {
                let label = if state.is_open() {
                    "Hide details"
                } else {
                    "Details"
                };
                let response = ui
                    .push_id(("details", step), |ui| {
                        ui.scope(|ui| {
                            ui.style_mut()
                                .text_styles
                                .insert(TextStyle::Body, text::caption());
                            theme::link(ui, label)
                        })
                        .inner
                    })
                    .inner;
                if response.clicked() {
                    state.toggle(ui);
                }
            }
        });
        state.show_body_unindented(ui, |ui| {
            for check in &settled {
                check_line(ui, check);
            }
        });
    }
}

/// How a row is animated this frame.
#[derive(Clone, Debug)]
struct DrawnRow {
    /// 0–1: the row rising in.
    appear: f32,
    /// 0–1: since its state last changed.
    pop: f32,
    /// The state it is changing from, while it changes.
    previous: Option<RowState>,
    /// 0–1: since its line of detail last changed.
    detail: f32,
    /// The previous state line, only while it fades away.
    old_detail: String,
}

/// A step's mark: in motion, the old mark shrinks away while the new one pops in, a finished
/// step sends out one ring, and a step waiting on something breathes.
fn draw_mark(
    painter: &egui::Painter,
    rect: Rect,
    state: RowState,
    motion: &DrawnRow,
    frame: &FrameState,
) {
    let mark = mark_of(state);
    let phase = if state == RowState::Working {
        frame.phase
    } else {
        None
    };
    let center = rect.center();
    let radius = rect.width() * 0.5;
    match motion.previous {
        Some(previous) if motion.pop < 1.0 => {
            let pop = motion.pop;
            let mut old = painter.clone();
            old.multiply_opacity(1.0 - easing::cubic_out((pop * 1.6).min(1.0)));
            let shrink = if frame.level.moves() { 0.35 * pop } else { 0.0 };
            theme::step_mark(
                &old,
                rect.shrink(rect.width() * shrink * 0.5),
                mark_of(previous),
                None,
            );
            let mut new = painter.clone();
            new.multiply_opacity(easing::cubic_out(pop));
            let scale = if frame.level.moves() {
                0.55 + 0.45 * easing::back_out(pop)
            } else {
                1.0
            };
            theme::step_mark(
                &new,
                Rect::from_center_size(center, rect.size() * scale),
                mark,
                phase,
            );
            if state == RowState::Verified && frame.level.moves() {
                painter.circle_stroke(
                    center,
                    radius * (1.0 + 0.8 * easing::cubic_out(pop)),
                    (1.5, theme::alpha(theme::FROST, (150.0 * (1.0 - pop)) as u8)),
                );
            }
        }
        _ => theme::step_mark(painter, rect, mark, phase),
    }
    if let Some(breath) = frame.breath
        && matches!(state, RowState::Waiting | RowState::NeedsAction)
    {
        let ease = easing::sin_in_out(breath);
        painter.circle_stroke(
            center,
            radius + 1.5 + 4.0 * ease,
            (1.0, theme::alpha(mark.color(), (90.0 * (1.0 - ease)) as u8)),
        );
    }
}

/// One checklist line: a small mark, the check, and its own wording.
fn check_line(ui: &mut egui::Ui, check: &RowView) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
        theme::step_mark(ui.painter(), rect.shrink(1.5), mark_of(check.state), None);
        let style = ui.style().clone();
        let font = egui::FontSelection::FontId(text::caption());
        let mut line = egui::text::LayoutJob::default();
        RichText::new(format!("{}  ", check.label))
            .color(theme::alpha(theme::ICE, 225))
            .append_to(&mut line, &style, font.clone(), egui::Align::Center);
        RichText::new(check_wording(check))
            .color(match check.state {
                RowState::Verified | RowState::Note => theme::QUIET,
                state => mark_of(state).color(),
            })
            .append_to(&mut line, &style, font, egui::Align::Center);
        ui.add(egui::Label::new(line).wrap());
    });
}

/// The demo badge, its bottom-right corner at `corner`.
fn demo_badge(painter: &egui::Painter, corner: Pos2) {
    let galley = painter.layout_no_wrap(DEMO_LABEL.to_owned(), text::small(), theme::WARNING);
    let size = galley.size() + Vec2::new(16.0, 6.0);
    let pill = Rect::from_min_size(corner - size, size);
    painter.rect(
        pill,
        9.0,
        theme::alpha(theme::WARNING, 18),
        (1.0, theme::alpha(theme::WARNING, 60)),
        egui::StrokeKind::Inside,
    );
    painter.galley(pill.min + Vec2::new(8.0, 3.0), galley, theme::WARNING);
}

/// "This computer: omarchy · Paired with mac-studio", or nothing.
fn machine_line(view: &WizardView) -> Option<String> {
    let machine = view.machine.as_ref().map(|machine| {
        if machine.starts_with("This ") {
            machine.clone()
        } else {
            format!("This computer: {machine}")
        }
    });
    let peer = view.peer.as_ref().map(|peer| format!("Paired with {peer}"));
    match (machine, peer) {
        (Some(machine), Some(peer)) => Some(format!("{machine}\n{peer}")),
        (machine, peer) => machine.or(peer),
    }
}

/// The header, measured before it is drawn.
#[derive(Clone, Copy, Debug)]
struct HeaderLayout {
    height: f32,
    eyebrow: f32,
}

impl WizardShell {
    fn header_layout(&self, ui: &egui::Ui, view: &WizardView, width: f32) -> HeaderLayout {
        let eyebrow = if self.narrow && (eyebrow(view, true).is_some() || view.demo) {
            20.0
        } else {
            0.0
        };
        let title = ui
            .painter()
            .layout(view.title.clone(), text::title(), theme::ICE, width)
            .size()
            .y;
        HeaderLayout {
            height: eyebrow + title,
            eyebrow,
        }
    }

    /// The header: where you are, then the question. The title cross-fades when it changes.
    fn header(
        &mut self,
        ui: &mut egui::Ui,
        view: &WizardView,
        layout: HeaderLayout,
        rect: Rect,
        frame: &FrameState,
    ) {
        let painter = ui.painter().with_clip_rect(rect.expand(4.0));
        if let Some(eyebrow) = eyebrow(view, self.narrow) {
            let mut job = egui::text::LayoutJob::default();
            job.append(
                &eyebrow,
                0.0,
                egui::TextFormat {
                    font_id: text::small(),
                    color: theme::QUIET,
                    extra_letter_spacing: 1.4,
                    ..Default::default()
                },
            );
            painter.galley(rect.min, painter.layout_job(job), theme::QUIET);
        }
        let title_at = Pos2::new(rect.left(), rect.top() + layout.eyebrow);
        // With a page, the old title fades out first and the new one rises in after it; on the
        // same page the two cross-fade.
        let duration = frame.level.duration(if self.title_with_page {
            TRANSITION_MS
        } else {
            TEXT_MS
        });
        let t = progress(frame.now, self.title.changed, duration);
        if t < 1.0 {
            self.wake_in(FRAME_MS);
        }
        let (fade_out, fade_in) = if self.title_with_page {
            (
                (t / 0.4).min(1.0),
                easing::cubic_out(((t - 0.3) / 0.7).clamp(0.0, 1.0)),
            )
        } else {
            (t, t)
        };
        let rise = if self.title_with_page && frame.level.moves() {
            6.0 * (1.0 - fade_in)
        } else {
            0.0
        };
        let galley = painter.layout(view.title.clone(), text::title(), theme::ICE, rect.width());
        let mut new = painter.clone();
        new.multiply_opacity(fade_in);
        new.galley(title_at + Vec2::new(0.0, rise), galley, theme::ICE);
        if fade_out < 1.0 && !self.title.old.is_empty() {
            let galley = painter.layout(
                self.title.old.clone(),
                text::title(),
                theme::ICE,
                rect.width(),
            );
            let mut old = painter.clone();
            old.multiply_opacity(1.0 - fade_out);
            old.galley(title_at, galley, theme::ICE);
        }
        let response = ui.interact(rect, ui.id().with("wizard-title"), Sense::hover());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Label, true, view.title.clone())
        });
    }
}

/// The small line over the title: the step, in narrow windows only (wide ones have the rail).
fn eyebrow(view: &WizardView, narrow: bool) -> Option<String> {
    if !narrow {
        return None;
    }
    match view.screen {
        ScreenId::Welcome | ScreenId::RepairRemove => None,
        _ => view.progress.current.map(|group| {
            let step = PROGRESS_GROUPS
                .iter()
                .position(|g| *g == group)
                .map_or(1, |index| index + 1);
            format!("STEP {step} OF {}", PROGRESS_GROUPS.len())
        }),
    }
}

/// One footer button, measured.
#[derive(Clone, Copy, Debug)]
struct FooterItem<'a> {
    button: &'a ButtonView,
    kind: ActionKind,
    width: f32,
    chevron: bool,
}

/// The footer, planned before drawing so the card knows its height: Back and the quiet
/// alternatives on the left as text buttons, the answers on the right with the primary one
/// last. A line that doesn't fit moves down whole; nothing ever wraps inside a button.
struct FooterPlan<'a> {
    lines: Vec<(Vec<FooterItem<'a>>, Vec<FooterItem<'a>>)>,
    width: f32,
    height: f32,
}

impl<'a> FooterPlan<'a> {
    fn new(ui: &egui::Ui, view: &'a WizardView, width: f32) -> Self {
        let spacing = space::S;
        let item = |button: &'a ButtonView, kind: ActionKind, chevron: bool| FooterItem {
            button,
            kind,
            width: theme::action_width(ui, &button.label, kind, chevron),
            chevron,
        };
        let mut left = Vec::new();
        let mut right = Vec::new();
        if let Some(back) = view
            .buttons
            .iter()
            .find(|button| button.role == ButtonRole::Back)
        {
            left.push(item(back, ActionKind::Ghost, true));
        }
        // Quiet alternatives without a heading of their own live in the footer, after Back.
        for button in view.buttons.iter().filter(|button| {
            button.role != ButtonRole::Back
                && inline_button_row(button.id).is_none()
                && (button.kind == ButtonKind::Secondary
                    || (button.kind == ButtonKind::Link
                        && (view.link_caption.is_none() || button.id == crate::live::ids::SKIP)))
        }) {
            left.push(item(button, ActionKind::Ghost, false));
        }
        let mut answers: Vec<&ButtonView> = view
            .buttons
            .iter()
            .filter(|button| {
                button.role != ButtonRole::Back
                    && inline_button_row(button.id).is_none()
                    && matches!(button.kind, ButtonKind::Primary | ButtonKind::Destructive)
            })
            .collect();
        // The primary action sits last, at the right edge.
        answers.sort_by_key(|button| button.kind == ButtonKind::Primary);
        for button in answers {
            let kind = match button.kind {
                ButtonKind::Primary => ActionKind::Primary,
                ButtonKind::Destructive => ActionKind::Destructive,
                _ => ActionKind::Secondary,
            };
            right.push(item(button, kind, false));
        }
        let row_width = |items: &[FooterItem<'_>]| {
            items.iter().map(|item| item.width).sum::<f32>()
                + spacing * items.len().saturating_sub(1) as f32
        };
        let mut lines: Vec<(Vec<FooterItem<'a>>, Vec<FooterItem<'a>>)> = Vec::new();
        // The left group stays together on the first line, as long as it fits at all.
        let mut current: (Vec<FooterItem<'a>>, Vec<FooterItem<'a>>) = (Vec::new(), Vec::new());
        for entry in left {
            if !current.0.is_empty() && row_width(&current.0) + spacing + entry.width > width {
                lines.push(std::mem::take(&mut current));
            }
            current.0.push(entry);
        }
        for entry in right {
            let used = row_width(&current.0)
                + if current.0.is_empty() { 0.0 } else { space::XL }
                + row_width(&current.1);
            let gap = if current.1.is_empty() { 0.0 } else { spacing };
            if (!current.0.is_empty() || !current.1.is_empty()) && used + gap + entry.width > width
            {
                lines.push(std::mem::take(&mut current));
            }
            current.1.push(entry);
        }
        if !current.0.is_empty() || !current.1.is_empty() {
            lines.push(current);
        }
        let count = lines.len();
        let height = if count == 0 {
            0.0
        } else {
            count as f32 * theme::ACTION_HEIGHT + (count - 1) as f32 * space::S
        };
        Self {
            lines,
            width,
            height,
        }
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    fn show(&self, ui: &mut egui::Ui, view: &WizardView, intents: &mut Vec<WizardIntent>) {
        let origin = ui.max_rect().min;
        ui.push_id(("wizard-actions", view.revision), |ui| {
            for (line, (left, right)) in self.lines.iter().enumerate() {
                let y = origin.y + line as f32 * (theme::ACTION_HEIGHT + space::S);
                let mut x = origin.x;
                for item in left {
                    let rect = Rect::from_min_size(
                        Pos2::new(x, y),
                        Vec2::new(item.width, theme::ACTION_HEIGHT),
                    );
                    footer_button(ui, view, item, rect, intents);
                    x += item.width + space::S;
                }
                let total: f32 = right.iter().map(|item| item.width).sum::<f32>()
                    + space::S * right.len().saturating_sub(1) as f32;
                let mut x = origin.x + self.width - total;
                for item in right {
                    let rect = Rect::from_min_size(
                        Pos2::new(x, y),
                        Vec2::new(item.width, theme::ACTION_HEIGHT),
                    );
                    footer_button(ui, view, item, rect, intents);
                    x += item.width + space::S;
                }
            }
        });
    }
}

fn footer_button(
    ui: &mut egui::Ui,
    view: &WizardView,
    item: &FooterItem<'_>,
    rect: Rect,
    intents: &mut Vec<WizardIntent>,
) {
    let button = item.button;
    // The D7 choice: its answer is inert until an option is picked.
    let enabled = button.enabled
        && !(view.screen == ScreenId::HidingChoice
            && button.role == ButtonRole::Next
            && view.hiding_choice.is_none());
    let response = ui
        .push_id(button.id, |ui| {
            let mut child = ui.new_child(UiBuilder::new().max_rect(rect));
            theme::action(&mut child, &button.label, item.kind, enabled, item.chevron)
        })
        .inner;
    if response.clicked() && enabled {
        intents.push(WizardIntent::Button(button.id));
    }
}

/// Quiet alternatives under their heading ("Other ways to connect"). Without a heading they
/// live in the footer.
fn links(ui: &mut egui::Ui, view: &WizardView, intents: &mut Vec<WizardIntent>) {
    let Some(caption) = &view.link_caption else {
        return;
    };
    let links: Vec<&ButtonView> = view
        .buttons
        .iter()
        .filter(|button| {
            button.kind == ButtonKind::Link
                && button.role != ButtonRole::Back
                && inline_button_row(button.id).is_none()
                && button.id != crate::live::ids::SKIP
        })
        .collect();
    if links.is_empty() {
        return;
    }
    ui.add_space(space::S);
    ui.label(
        RichText::new(caption)
            .font(text::caption())
            .color(theme::QUIET),
    );
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

impl WizardShell {
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
                    if !self.outgoing_pass {
                        self.address_focus.push((*id, response.id));
                    }
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
                        // Sub-pixel scrollbar rounding is not clipping.
                        let withheld_keys = if ui.clip_rect().expand(1.0).contains_rect(toolbar) {
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

/// Answers to pick from: numbers side by side, statements and computers one under another.
fn choices(ui: &mut egui::Ui, view: &WizardView, intents: &mut Vec<WizardIntent>) {
    let choices: Vec<&ButtonView> = view
        .buttons
        .iter()
        .filter(|button| {
            button.kind == ButtonKind::Choice && inline_button_row(button.id).is_none()
        })
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
        RowState::Note | RowState::Skipped => StepMark::Info,
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
        RowState::Skipped => "skipped",
    }
}

/// Every action has a state line; informational rows may have no extra text.
fn shows_detail(row: &RowView) -> bool {
    row.state != RowState::Note || !row.detail.trim().is_empty()
}

/// A permission row's actions, under its words: its one filled "Allow" and quiet links.
fn row_actions(
    ui: &mut egui::Ui,
    view: &WizardView,
    actions: &[&ButtonView],
    intents: &mut Vec<WizardIntent>,
) {
    if actions.is_empty() {
        return;
    }
    let width = ui.available_width() - 34.0;
    let mut lines: Vec<Vec<&ButtonView>> = vec![Vec::new()];
    let mut used = 0.0;
    for button in actions {
        let text_width = ui
            .painter()
            .layout_no_wrap(
                button.label.clone(),
                egui::TextStyle::Body.resolve(ui.style()),
                theme::GLACIER,
            )
            .size()
            .x;
        let padding = if button.kind == ButtonKind::Link {
            4.0
        } else {
            2.0 * ui.spacing().button_padding.x
        };
        let item_width = text_width + padding;
        if used > 0.0 && used + 16.0 + item_width > width {
            lines.push(Vec::new());
            used = 0.0;
        }
        if let Some(line) = lines.last_mut() {
            line.push(button);
        }
        used += if used > 0.0 { 16.0 } else { 0.0 } + item_width;
    }
    for line in lines {
        ui.horizontal(|ui| {
            // Line up every wrapped line with the row's words, past its mark.
            ui.add_space(34.0);
            ui.spacing_mut().item_spacing.x = 16.0;
            for button in line {
                let response = ui
                    .push_id(("row-action", view.revision, button.id), |ui| {
                        ui.add_enabled_ui(button.enabled, |ui| match button.kind {
                            ButtonKind::Link => theme::link(ui, &button.label),
                            _ => theme::primary(ui, &button.label, button.enabled),
                        })
                        .inner
                    })
                    .inner;
                reveal_focus(&response);
                if response.clicked()
                    && button.enabled
                    && ui.clip_rect().contains_rect(response.rect)
                {
                    intents.push(WizardIntent::Button(button.id));
                }
            }
        });
    }
}

fn row_detail(row: &RowView) -> String {
    match row.state {
        RowState::Skipped => format!("Skipped. {}", row.detail).trim().to_owned(),
        RowState::Verified => "Done".into(),
        RowState::Unchecked => "Not started yet".into(),
        RowState::Working if row.label == "Install Crosspane" => "Installing…".into(),
        RowState::Working if row.label == "Restart Crosspane" => "Restarting…".into(),
        RowState::Working if row.label == "Start Crosspane when you sign in" => {
            "Enabling startup…".into()
        }
        RowState::Working if row.detail.trim().is_empty() => match row.label.as_str() {
            label if label.starts_with("Check ") => "Checking…",
            _ => "In progress…",
        }
        .into(),
        RowState::NeedsAction if row.detail.trim().is_empty() => "Your answer is needed".into(),
        RowState::Waiting if row.detail.trim().is_empty() => "Waiting for confirmation…".into(),
        RowState::Failed if row.detail.trim().is_empty() => "This step did not finish".into(),
        RowState::Unsupported if row.detail.trim().is_empty() => {
            "Unavailable on this computer".into()
        }
        _ => row.detail.clone(),
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
        RowState::Skipped => with("Skipped"),
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

/// Illustrations that only decorate give way when the card is short, so the controls come first;
/// the ones that carry a fact (the number to compare, traffic, a privacy note) always stay. The
/// welcome drawing is the page's only content besides its sentence, so it stays longest.
fn gives_way(screen: ScreenId, room: f32) -> bool {
    match screen {
        ScreenId::Welcome => room < 240.0,
        ScreenId::Grants | ScreenId::HidingChoice | ScreenId::Practice | ScreenId::Summary => {
            room < 400.0
        }
        _ => false,
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
        ScreenId::Welcome => 132.0,
        ScreenId::MatchNumbers => 104.0,
        ScreenId::HidingChoice | ScreenId::Practice => 96.0,
        ScreenId::Summary => 64.0,
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
    // No box around it: a soft light behind the drawing is enough.
    theme::radial_glow(
        &painter,
        bounds.center(),
        bounds.width().min(bounds.height() * 3.0) * 0.5,
        theme::alpha(theme::NAVY, 70),
    );
    let compact = height <= 120.0;
    let content = bounds.shrink2(if compact {
        egui::vec2(12.0, 10.0)
    } else {
        egui::vec2(20.0, 16.0)
    });
    // Only the notes that say something the screen doesn't: privacy and what is illustrative.
    let noted = matches!(
        view.screen,
        ScreenId::Permissions | ScreenId::AudioComponent | ScreenId::Network
    );
    let note_height = match (noted, compact) {
        (false, _) => 0.0,
        (true, true) => 22.0,
        (true, false) => 28.0,
    };
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
