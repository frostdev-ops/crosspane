//! Crosspane's dark-only palette, glass surfaces and small shape-based controls.
//!
//! Nothing here loads fonts or art: the caller configures fonts on the [`egui::Context`] first.
use egui::{Color32, FontId, Pos2, Rect, Response, RichText, Sense, Stroke, StrokeKind, Vec2};

/// Backgrounds.
pub const MIDNIGHT: Color32 = Color32::from_rgb(0x07, 0x15, 0x25);
/// Structure and surfaces.
pub const NAVY: Color32 = Color32::from_rgb(0x16, 0x4a, 0x74);
/// Primary accent and actions.
pub const FROST: Color32 = Color32::from_rgb(0x17, 0xc8, 0xf4);
/// Highlights and hover.
pub const GLACIER: Color32 = Color32::from_rgb(0x6f, 0xdc, 0xff);
/// Primary text.
pub const ICE: Color32 = Color32::from_rgb(0xe9, 0xf8, 0xff);
/// The tint of peer machines on the desk.
pub const PEER_ICE: Color32 = Color32::from_rgb(0xb7, 0xef, 0xff);
/// Secondary text and badges.
pub const QUIET: Color32 = Color32::from_rgb(0x89, 0xcb, 0xd5);
/// Warnings, errors and destructive actions.
pub const WARNING: Color32 = Color32::from_rgb(0xd9, 0x88, 0x91);

/// `color` with the given unmultiplied opacity.
pub fn alpha(color: Color32, opacity: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), opacity)
}

/// The dark egui style every Crosspane window installs (`set_style_of(Theme::Dark, …)`).
pub fn style() -> egui::Style {
    let mut style = egui::Style {
        visuals: egui::Visuals::dark(),
        ..Default::default()
    };
    style.visuals.override_text_color = Some(ICE);
    style.visuals.panel_fill = MIDNIGHT;
    style.visuals.window_fill = Color32::from_rgb(10, 30, 48);
    style.visuals.extreme_bg_color = MIDNIGHT;
    style.visuals.faint_bg_color = alpha(NAVY, 100);
    style.visuals.selection.bg_fill = alpha(FROST, 65);
    style.visuals.selection.stroke = Stroke::new(1.0, GLACIER);
    style.visuals.hyperlink_color = GLACIER;
    style.visuals.warn_fg_color = WARNING;
    style.visuals.error_fg_color = WARNING;
    for (widget, fill, border) in [
        (
            &mut style.visuals.widgets.noninteractive,
            MIDNIGHT,
            alpha(GLACIER, 35),
        ),
        (
            &mut style.visuals.widgets.inactive,
            alpha(NAVY, 95),
            alpha(GLACIER, 65),
        ),
        (&mut style.visuals.widgets.hovered, NAVY, GLACIER),
        (
            &mut style.visuals.widgets.active,
            Color32::from_rgb(8, 62, 91),
            FROST,
        ),
        (&mut style.visuals.widgets.open, NAVY, FROST),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::new(1.0, border);
        widget.fg_stroke = Stroke::new(1.0, ICE);
        widget.corner_radius = 8.into();
        widget.expansion = 0.0;
    }
    style.spacing.item_spacing = Vec2::new(12.0, 10.0);
    style.spacing.button_padding = Vec2::new(16.0, 9.0);
    style.spacing.interact_size.y = 34.0;
    style.animation_time = 0.16;
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, FontId::proportional(24.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, FontId::proportional(11.0));
    style
}

/// A frosted-glass panel: translucent Midnight, a faint Glacier border and a soft shadow.
pub fn glass() -> egui::Frame {
    egui::Frame::new()
        .fill(alpha(MIDNIGHT, 205))
        .stroke(Stroke::new(1.0, alpha(GLACIER, 42)))
        .corner_radius(12)
        .inner_margin(20)
        .shadow(egui::epaint::Shadow {
            offset: [0, 6],
            blur: 22,
            spread: 0,
            color: Color32::from_black_alpha(75),
        })
}

/// A small letter-spaced section label in Quiet cyan.
pub fn section(ui: &mut egui::Ui, text: &str) {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: FontId::proportional(11.0),
            color: QUIET,
            extra_letter_spacing: 1.6,
            ..Default::default()
        },
    );
    ui.label(job);
}

/// A Frost-cyan primary button with Midnight text; muted and inert when `enabled` is false.
pub fn primary(ui: &mut egui::Ui, text: &str, enabled: bool) -> Response {
    if !enabled || !ui.is_enabled() {
        return ui
            .scope(|ui| {
                ui.visuals_mut().disabled_alpha = 1.0;
                ui.add_enabled(
                    false,
                    egui::Button::new(RichText::new(text).strong().color(QUIET))
                        .fill(alpha(NAVY, 38))
                        .stroke(Stroke::new(1.0, alpha(QUIET, 32))),
                )
            })
            .inner;
    }
    ui.scope(|ui| {
        ui.visuals_mut().override_text_color = Some(MIDNIGHT);
        let widgets = &mut ui.style_mut().visuals.widgets;
        for (widget, color) in [
            (&mut widgets.inactive, FROST),
            (&mut widgets.hovered, GLACIER),
            (&mut widgets.active, Color32::from_rgb(8, 151, 192)),
        ] {
            widget.weak_bg_fill = color;
            widget.bg_fill = color;
            widget.fg_stroke = Stroke::new(1.0, MIDNIGHT);
            widget.bg_stroke = Stroke::NONE;
        }
        ui.add_enabled(enabled, egui::Button::new(RichText::new(text).strong()))
    })
    .inner
}

/// All editable fields share button-sized padding and the glass/focused border treatment.
pub fn text_edit(ui: &mut egui::Ui, text: &mut String, placeholder: &str) -> Response {
    ui.scope(|ui| {
        let visuals = ui.visuals_mut();
        visuals.weak_text_color = Some(QUIET);
        visuals.selection.stroke = Stroke::new(1.0, FROST);
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, alpha(QUIET, 60));
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, alpha(GLACIER, 130));
        ui.add(
            egui::TextEdit::singleline(text)
                .hint_text(placeholder.to_owned())
                .text_color(ICE)
                .background_color(alpha(NAVY, 45))
                .margin(egui::Margin::symmetric(12, 9))
                .desired_width(f32::INFINITY),
        )
    })
    .inner
}

/// The Glacier glow along an edge where two machines' displays touch, optionally with a small
/// two-way arrow marker at its middle. It is illustrative: it never claims an open gate or an
/// armed crossing.
pub fn crossing_glow(painter: &egui::Painter, line: [Pos2; 2], marker: bool) {
    for (width, opacity) in [(16.0, 12), (10.0, 28), (6.0, 60), (3.5, 255)] {
        painter.line_segment(line, Stroke::new(width, alpha(GLACIER, opacity)));
    }
    if marker {
        let center = line[0].lerp(line[1], 0.5);
        let tangent = (line[1] - line[0]).normalized();
        let normal = Vec2::new(-tangent.y, tangent.x);
        painter.circle_filled(center, 10.0, alpha(MIDNIGHT, 235));
        painter.circle_stroke(center, 10.0, Stroke::new(1.0, alpha(GLACIER, 100)));
        // Two opposed arrows across the edge (the shape equivalent of ⇄).
        for direction in [-1.0, 1.0] {
            let middle = center + tangent * (direction * 2.5);
            let tip = middle + normal * (direction * 6.0);
            let tail = middle - normal * (direction * 6.0);
            let stroke = Stroke::new(1.2, GLACIER);
            painter.line_segment([tail, tip], stroke);
            for side in [-1.0, 1.0] {
                painter.line_segment(
                    [
                        tip,
                        tip - normal * (direction * 3.0) + tangent * (side * 2.5),
                    ],
                    stroke,
                );
            }
        }
    }
}

/// The legend chip that explains the glowing edges.
pub fn crossing_legend(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(alpha(QUIET, 18))
        .stroke(Stroke::new(1.0, alpha(QUIET, 55)))
        .corner_radius(20)
        .inner_margin(egui::Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.spacing_mut().interact_size.y = 14.0;
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(20.0, 14.0), Sense::hover());
                crossing_glow(
                    ui.painter(),
                    [rect.left_center(), rect.right_center()],
                    false,
                );
                ui.label(
                    RichText::new("Glowing edges · pointer crossings")
                        .size(11.0)
                        .color(QUIET),
                );
            });
        });
}

/// An outlined button in the muted warning red.
pub fn destructive(ui: &mut egui::Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(WARNING))
            .stroke(Stroke::new(1.0, alpha(WARNING, 85))),
    )
}

/// A small pill-shaped label tinted with `color`.
pub fn chip(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(alpha(color, 18))
        .stroke(Stroke::new(1.0, alpha(color, 55)))
        .corner_radius(20)
        .inner_margin(egui::Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).color(color));
        });
}

/// A labelled pill switch. The response is marked changed when the value flips.
pub fn switch(ui: &mut egui::Ui, value: &mut bool, label: &str) -> Response {
    let width = ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(label.into(), FontId::proportional(14.0), ICE)
            .size()
            .x
    });
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::new(48.0 + width, 28.0), Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *value, label)
    });
    let t = ui.ctx().animate_bool_responsive(response.id, *value);
    let track = Rect::from_min_size(rect.min + Vec2::new(0.0, 4.0), Vec2::new(36.0, 20.0));
    ui.painter().rect_filled(
        track,
        10.0,
        if *value {
            alpha(FROST, 75)
        } else {
            alpha(NAVY, 170)
        },
    );
    ui.painter().rect_stroke(
        track,
        10.0,
        Stroke::new(1.0, if *value { FROST } else { alpha(QUIET, 85) }),
        StrokeKind::Inside,
    );
    ui.painter().circle_filled(
        Pos2::new(track.left() + 10.0 + 16.0 * t, track.center().y),
        6.0,
        if *value { GLACIER } else { QUIET },
    );
    ui.painter().text(
        rect.min + Vec2::new(46.0, 14.0),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.0),
        ICE,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A vertical gradient triangulated around a rounded perimeter, without a rectangular corner fill.
pub fn gradient(painter: &egui::Painter, rect: Rect, radius: f32, top: Color32, bottom: Color32) {
    if !rect.is_positive() {
        return;
    }
    let radius = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
    let color = |y: f32| {
        let t = ((y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        let a = top.to_array();
        let b = bottom.to_array();
        Color32::from_rgba_premultiplied(
            (f32::from(a[0]) * (1.0 - t) + f32::from(b[0]) * t) as u8,
            (f32::from(a[1]) * (1.0 - t) + f32::from(b[1]) * t) as u8,
            (f32::from(a[2]) * (1.0 - t) + f32::from(b[2]) * t) as u8,
            (f32::from(a[3]) * (1.0 - t) + f32::from(b[3]) * t) as u8,
        )
    };
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.center(), color(rect.center().y));
    for (center, angle) in [
        (
            Pos2::new(rect.right() - radius, rect.top() + radius),
            -90.0_f32,
        ),
        (
            Pos2::new(rect.right() - radius, rect.bottom() - radius),
            0.0,
        ),
        (
            Pos2::new(rect.left() + radius, rect.bottom() - radius),
            90.0,
        ),
        (Pos2::new(rect.left() + radius, rect.top() + radius), 180.0),
    ] {
        for step in 0..=6 {
            let radians = (angle + step as f32 * 15.0).to_radians();
            let point = center + radius * Vec2::new(radians.cos(), radians.sin());
            mesh.colored_vertex(point, color(point.y));
        }
    }
    for i in 1..=28 {
        mesh.add_triangle(0, i, if i == 28 { 1 } else { i + 1 });
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// Four small navigation symbols, built from lines and rectangles rather than font glyphs.
pub fn icon(painter: &egui::Painter, rect: Rect, index: usize, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    let r = rect.shrink(3.0);
    match index {
        0 => {
            painter.rect_stroke(
                r.shrink2(Vec2::new(0.0, 3.0)),
                2.0,
                stroke,
                StrokeKind::Inside,
            );
            painter.line_segment(
                [r.center_bottom(), r.center_bottom() - Vec2::new(0.0, 3.0)],
                stroke,
            );
            painter.line_segment(
                [
                    r.center_bottom() - Vec2::new(4.0, 0.0),
                    r.center_bottom() + Vec2::new(4.0, 0.0),
                ],
                stroke,
            );
        }
        1 => {
            for offset in [Vec2::ZERO, Vec2::new(9.0, 4.0)] {
                painter.rect_stroke(
                    Rect::from_min_size(r.min + offset, Vec2::new(8.0, 10.0)),
                    1.0,
                    stroke,
                    StrokeKind::Inside,
                );
            }
        }
        2 => {
            painter.circle_stroke(r.center() - Vec2::new(4.0, 0.0), 5.0, stroke);
            painter.circle_stroke(r.center() + Vec2::new(4.0, 0.0), 5.0, stroke);
        }
        _ => {
            painter.rect_stroke(
                r.translate(Vec2::new(3.0, -2.0)).shrink(2.0),
                2.0,
                stroke,
                StrokeKind::Inside,
            );
            painter.rect_filled(r.translate(Vec2::new(-2.0, 2.0)).shrink(2.0), 2.0, MIDNIGHT);
            painter.rect_stroke(
                r.translate(Vec2::new(-2.0, 2.0)).shrink(2.0),
                2.0,
                stroke,
                StrokeKind::Inside,
            );
        }
    }
}

/// The state of one step in a checklist. Every mark has its own shape, so a state never rests on
/// colour alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepMark {
    /// Not started: a faint ring.
    Pending,
    /// Running now: a ring with a bright arc (it turns only when a phase is supplied).
    Active,
    /// The question being asked now: a ring around a filled dot.
    Current,
    /// Done: a filled disc with a check.
    Done,
    /// Waiting on something outside this window: a ring with clock hands.
    Waiting,
    /// Stopped with a problem the person can fix: a ring with an exclamation mark.
    Problem,
    /// Can't be done here: a ring with a cross.
    Blocked,
    /// Information only: a ring with a small "i".
    Info,
}

impl StepMark {
    /// The mark's colour in the Crosspane palette.
    pub fn color(self) -> Color32 {
        match self {
            Self::Pending => alpha(QUIET, 120),
            Self::Active | Self::Current | Self::Done => FROST,
            Self::Waiting => GLACIER,
            Self::Problem | Self::Blocked => WARNING,
            Self::Info => QUIET,
        }
    }
}

/// Draws a [`StepMark`] centred in `rect`. `phase` (0–1) turns the [`StepMark::Active`] arc; with
/// `None` the arc stays still, which is how reduced motion shows work in progress.
pub fn step_mark(painter: &egui::Painter, rect: Rect, mark: StepMark, phase: Option<f32>) {
    let color = mark.color();
    let center = rect.center();
    let radius = rect.width().min(rect.height()) * 0.5 - 1.5;
    let thin = Stroke::new(1.5, color);
    match mark {
        StepMark::Pending => {
            painter.circle_stroke(center, radius, Stroke::new(1.5, color));
        }
        StepMark::Active => {
            painter.circle_stroke(center, radius, Stroke::new(2.0, alpha(FROST, 45)));
            let start = phase.unwrap_or(0.0) * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            let points: Vec<Pos2> = (0..=16)
                .map(|i| {
                    let angle = start + i as f32 / 16.0 * std::f32::consts::PI * 1.3;
                    center + radius * Vec2::new(angle.cos(), angle.sin())
                })
                .collect();
            painter.add(egui::Shape::line(points, Stroke::new(2.0, FROST)));
        }
        StepMark::Current => {
            painter.circle_stroke(center, radius, Stroke::new(1.5, color));
            painter.circle_filled(center, radius * 0.45, color);
        }
        StepMark::Done => {
            painter.circle_filled(center, radius, color);
            let r = radius * 0.5;
            painter.add(egui::Shape::line(
                vec![
                    center + Vec2::new(-r, 0.05 * r),
                    center + Vec2::new(-0.25 * r, 0.75 * r),
                    center + Vec2::new(r, -0.6 * r),
                ],
                Stroke::new(2.0, MIDNIGHT),
            ));
        }
        StepMark::Waiting => {
            painter.circle_stroke(center, radius, thin);
            painter.line_segment([center, center - Vec2::new(0.0, radius * 0.55)], thin);
            painter.line_segment([center, center + Vec2::new(radius * 0.45, 0.0)], thin);
        }
        StepMark::Problem => {
            painter.circle_stroke(center, radius, thin);
            painter.line_segment(
                [
                    center - Vec2::new(0.0, radius * 0.5),
                    center + Vec2::new(0.0, radius * 0.12),
                ],
                Stroke::new(2.0, color),
            );
            painter.circle_filled(center + Vec2::new(0.0, radius * 0.45), 1.3, color);
        }
        StepMark::Blocked => {
            painter.circle_stroke(center, radius, thin);
            let r = radius * 0.4;
            painter.line_segment([center + Vec2::new(-r, -r), center + Vec2::new(r, r)], thin);
            painter.line_segment([center + Vec2::new(r, -r), center + Vec2::new(-r, r)], thin);
        }
        StepMark::Info => {
            painter.circle_stroke(center, radius, thin);
            painter.line_segment(
                [
                    center - Vec2::new(0.0, radius * 0.1),
                    center + Vec2::new(0.0, radius * 0.5),
                ],
                thin,
            );
            painter.circle_filled(center - Vec2::new(0.0, radius * 0.42), 1.2, color);
        }
    }
}

/// How lit a control is, 0–1, eased over the style's animation time.
fn glow(ui: &egui::Ui, response: &Response, lit: bool) -> f32 {
    ui.ctx()
        .animate_bool_with_time(response.id.with("glow"), lit, ui.style().animation_time)
}

fn focus_ring(ui: &egui::Ui, response: &Response, rect: Rect, radius: f32) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(3.0),
            radius + 3.0,
            Stroke::new(1.5, GLACIER),
            StrokeKind::Outside,
        );
    }
}

/// A quiet text link for secondary actions: Glacier text, underlined while hovered or focused,
/// never wrapped. Muted and inert when the surrounding `Ui` is disabled.
pub fn link(ui: &mut egui::Ui, text: &str) -> Response {
    let enabled = ui.is_enabled();
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::TextStyle::Body.resolve(ui.style()),
        GLACIER,
    );
    let padding = Vec2::new(2.0, 6.0);
    let (rect, response) = ui.allocate_exact_size(galley.size() + 2.0 * padding, Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, enabled, text));
    if ui.is_rect_visible(rect) {
        let lit = enabled && (response.hovered() || response.has_focus());
        let t = glow(ui, &response, lit);
        let color = if !enabled {
            alpha(QUIET, 110)
        } else {
            mix(GLACIER, ICE, t)
        };
        let origin = rect.min + padding;
        let underline_y = origin.y + galley.size().y + 1.0;
        let width = galley.size().x;
        ui.painter().galley(origin, galley, color);
        if t > 0.0 {
            // The underline draws in from the left as the pointer arrives.
            ui.painter().line_segment(
                [
                    Pos2::new(origin.x, underline_y),
                    Pos2::new(origin.x + width * t, underline_y),
                ],
                Stroke::new(1.0, alpha(color, (255.0 * t) as u8)),
            );
        }
        focus_ring(ui, &response, rect, 4.0);
    }
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

/// An outlined secondary button whose text never wraps.
pub fn secondary(ui: &mut egui::Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(ICE))
            .wrap_mode(egui::TextWrapMode::Extend)
            .fill(alpha(NAVY, 70))
            .stroke(Stroke::new(1.0, alpha(GLACIER, 90))),
    )
}

/// A selectable tile for choosing one answer among several (a number to match, a computer to
/// pair with, a statement to confirm). `large` draws short labels big and centred, for numbers.
/// The tile is `width` wide; its height follows the wrapped label.
pub fn choice(ui: &mut egui::Ui, label: &str, width: f32, large: bool) -> Response {
    let enabled = ui.is_enabled();
    let font = if large {
        FontId::proportional(30.0)
    } else {
        egui::TextStyle::Body.resolve(ui.style())
    };
    let padding = if large {
        Vec2::new(16.0, 16.0)
    } else {
        Vec2::new(16.0, 13.0)
    };
    let mark = if large { 0.0 } else { 26.0 };
    let wrap = (width - 2.0 * padding.x - mark).max(40.0);
    let galley = ui.painter().layout(label.to_owned(), font, ICE, wrap);
    let height = galley.size().y + 2.0 * padding.y;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        let lit = enabled && (response.hovered() || response.has_focus());
        let pressed = enabled && response.is_pointer_button_down_on();
        let t = glow(ui, &response, lit);
        let fill = if pressed {
            alpha(FROST, 60)
        } else {
            mix(alpha(NAVY, 90), alpha(NAVY, 190), t)
        };
        let border = if !enabled {
            alpha(QUIET, 40)
        } else {
            mix(alpha(GLACIER, 80), FROST, t)
        };
        ui.painter().rect_filled(rect, 10.0, fill);
        ui.painter()
            .rect_stroke(rect, 10.0, Stroke::new(1.0, border), StrokeKind::Inside);
        let text_color = if enabled { ICE } else { alpha(QUIET, 140) };
        if large {
            let at = rect.center() - galley.size() * 0.5;
            ui.painter().galley(at, galley, text_color);
        } else {
            let ring = Rect::from_center_size(
                Pos2::new(rect.left() + padding.x + 8.0, rect.top() + padding.y + 9.0),
                Vec2::splat(16.0),
            );
            ui.painter().circle_stroke(
                ring.center(),
                7.0,
                Stroke::new(1.5, mix(alpha(GLACIER, 160), FROST, t)),
            );
            if pressed {
                ui.painter().circle_filled(ring.center(), 3.5, FROST);
            }
            let at = Pos2::new(rect.left() + padding.x + mark, rect.top() + padding.y);
            ui.painter().galley(at, galley, text_color);
        }
        focus_ring(ui, &response, rect, 10.0);
    }
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

/// One option of a single choice, drawn as a tile with a radio ring. The ring is outlined in
/// Glacier even when nothing is chosen, so an unselected option is always visibly selectable.
pub fn option(ui: &mut egui::Ui, selected: bool, label: &str, width: f32) -> Response {
    let enabled = ui.is_enabled();
    let padding = Vec2::new(16.0, 14.0);
    let ring_space = 26.0;
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley = ui.painter().layout(
        label.to_owned(),
        font,
        ICE,
        (width - 2.0 * padding.x - ring_space).max(40.0),
    );
    let height = galley.size().y.max(18.0) + 2.0 * padding.y;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::RadioButton, enabled, selected, label)
    });
    if ui.is_rect_visible(rect) {
        let lit = enabled && (response.hovered() || response.has_focus());
        let t = glow(ui, &response, lit);
        let chosen = ui.ctx().animate_bool_with_time(
            response.id.with("chosen"),
            selected,
            ui.style().animation_time * 1.5,
        );
        let fill = mix(
            mix(alpha(NAVY, 90), alpha(NAVY, 180), t),
            alpha(FROST, 30),
            chosen,
        );
        let border = mix(
            mix(alpha(GLACIER, 70), alpha(GLACIER, 200), t),
            FROST,
            chosen,
        );
        ui.painter().rect_filled(rect, 10.0, fill);
        ui.painter()
            .rect_stroke(rect, 10.0, Stroke::new(1.0, border), StrokeKind::Inside);
        let ring = Pos2::new(rect.left() + padding.x + 9.0, rect.top() + padding.y + 9.0);
        ui.painter().circle_stroke(
            ring,
            8.0,
            Stroke::new(1.5, if selected { FROST } else { GLACIER }),
        );
        if chosen > 0.0 {
            ui.painter().circle_filled(ring, 4.0 * chosen, FROST);
        }
        ui.painter().galley(
            Pos2::new(rect.left() + padding.x + ring_space, rect.top() + padding.y),
            galley,
            if enabled { ICE } else { alpha(QUIET, 140) },
        );
        focus_ring(ui, &response, rect, 10.0);
    }
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

/// A labelled on/off switch whose label wraps to `width`. It reports clicks and never flips
/// `on` itself: the caller decides. Off, the track is outlined in Glacier, so it reads as a
/// control without hovering.
pub fn toggle(ui: &mut egui::Ui, on: bool, label: &str, width: f32) -> Response {
    let enabled = ui.is_enabled();
    let track_size = Vec2::new(38.0, 22.0);
    let gap = 10.0;
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley = ui.painter().layout(
        label.to_owned(),
        font,
        ICE,
        (width - track_size.x - gap).max(40.0),
    );
    let height = galley.size().y.max(track_size.y) + 8.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, on, label));
    if ui.is_rect_visible(rect) {
        let lit = enabled && (response.hovered() || response.has_focus());
        let slide = ui.ctx().animate_bool_with_time(
            response.id.with("on"),
            on,
            // Reduced-motion layouts disable geometric animation but keep short color fades.
            if ui.style().scroll_animation.duration.max > 0.0 {
                ui.style().animation_time * 1.4
            } else {
                0.0
            },
        );
        let track = Rect::from_min_size(Pos2::new(rect.left(), rect.top() + 4.0), track_size);
        ui.painter().rect_filled(
            track,
            11.0,
            if on {
                alpha(FROST, 90)
            } else {
                alpha(NAVY, 150)
            },
        );
        ui.painter().rect_stroke(
            track,
            11.0,
            Stroke::new(
                1.5,
                if on {
                    FROST
                } else if !enabled {
                    alpha(QUIET, 90)
                } else {
                    GLACIER
                },
            ),
            StrokeKind::Inside,
        );
        let knob_x = track.left() + 11.0 + (track.width() - 22.0) * slide;
        ui.painter().circle_filled(
            Pos2::new(knob_x, track.center().y),
            7.0,
            if on { ICE } else { alpha(QUIET, 150) },
        );
        let text_color = if !enabled {
            alpha(QUIET, 140)
        } else if lit {
            ICE
        } else {
            alpha(ICE, 235)
        };
        ui.painter().galley(
            Pos2::new(track.right() + gap, rect.top() + 4.0 + 1.0),
            galley,
            text_color,
        );
        focus_ring(ui, &response, rect, 6.0);
    }
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

/// A slim segmented progress strip: `done` segments filled, the `current` one highlighted.
pub fn progress_strip(painter: &egui::Painter, rect: Rect, done: &[bool], current: Option<usize>) {
    let count = done.len().max(1) as f32;
    let gap = 6.0;
    let width = ((rect.width() - gap * (count - 1.0)) / count).max(2.0);
    for (index, finished) in done.iter().enumerate() {
        let left = rect.left() + index as f32 * (width + gap);
        let segment =
            Rect::from_min_size(Pos2::new(left, rect.top()), Vec2::new(width, rect.height()));
        let color = if *finished {
            FROST
        } else if current == Some(index) {
            GLACIER
        } else {
            alpha(QUIET, 55)
        };
        painter.rect_filled(segment, rect.height() * 0.5, color);
    }
}

/// A small gear, for the settings affordance.
pub fn gear(painter: &egui::Painter, rect: Rect, color: Color32) {
    let center = rect.center();
    let radius = rect.width().min(rect.height()) * 0.5;
    let stroke = Stroke::new(1.5, color);
    for tooth in 0..8 {
        let angle = tooth as f32 * std::f32::consts::TAU / 8.0;
        let direction = Vec2::new(angle.cos(), angle.sin());
        painter.line_segment(
            [
                center + direction * radius * 0.62,
                center + direction * radius * 0.95,
            ],
            Stroke::new(2.4, color),
        );
    }
    painter.circle_stroke(center, radius * 0.62, stroke);
    painter.circle_stroke(center, radius * 0.25, stroke);
}

// ---------------------------------------------------------------------------------------------
// Onboarding tokens and controls (WP-4.34). Additive: the settings app keeps the controls above.
// ---------------------------------------------------------------------------------------------

/// Spacing tokens, in points. Layouts use these instead of ad-hoc numbers.
pub mod space {
    /// Hairline gaps: a mark and its label.
    pub const XS: f32 = 4.0;
    /// Inside a group.
    pub const S: f32 = 8.0;
    /// Between related items.
    pub const M: f32 = 12.0;
    /// Between items of a list.
    pub const L: f32 = 16.0;
    /// Between groups.
    pub const XL: f32 = 24.0;
    /// Around a surface.
    pub const XXL: f32 = 32.0;
}

/// The type scale: one title size, body, caption, a small label and mono for exact commands.
pub mod text {
    use egui::FontId;

    /// A screen's title.
    pub fn title() -> FontId {
        FontId::proportional(25.0)
    }
    /// Running text and step titles.
    pub fn body() -> FontId {
        FontId::proportional(15.0)
    }
    /// A step's state line and other secondary text.
    pub fn caption() -> FontId {
        FontId::proportional(13.0)
    }
    /// Eyebrows and badges.
    pub fn small() -> FontId {
        FontId::proportional(11.5)
    }
    /// Exact commands.
    pub fn mono() -> FontId {
        FontId::monospace(13.0)
    }
    /// The line height of running text.
    pub const BODY_LINE: f32 = 22.0;
}

/// The onboarding card's fill: nearly opaque, so text never fights the backdrop.
pub fn card_fill() -> Color32 {
    Color32::from_rgba_unmultiplied(8, 22, 38, 236)
}

/// The onboarding card's edge.
pub fn card_stroke() -> Color32 {
    alpha(GLACIER, 34)
}

/// `a` blended towards `b` by `t` (0–1), on unmultiplied channels. Exactly `a` at 0 and `b` at 1.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    if t <= 0.0 {
        return a;
    }
    if t >= 1.0 {
        return b;
    }
    let [ar, ag, ab, aa] = a.to_srgba_unmultiplied();
    let [br, bg, bb, ba] = b.to_srgba_unmultiplied();
    let channel = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        channel(ar, br),
        channel(ag, bg),
        channel(ab, bb),
        channel(aa, ba),
    )
}

/// A soft round glow: `color` at the centre fading to nothing at `radius`.
pub fn radial_glow(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    if radius <= 0.0 || color.a() == 0 {
        return;
    }
    const SEGMENTS: u32 = 48;
    let middle = mix(alpha(color, 0), color, 0.38);
    let clear = alpha(color, 0);
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(center, color);
    for ring in [(0.45, middle), (1.0, clear)] {
        for i in 0..SEGMENTS {
            let angle = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            mesh.colored_vertex(
                center + Vec2::new(angle.cos(), angle.sin()) * radius * ring.0,
                ring.1,
            );
        }
    }
    for i in 0..SEGMENTS {
        let next = (i + 1) % SEGMENTS;
        let (a, b) = (1 + i, 1 + next);
        mesh.add_triangle(0, a, b);
        let (c, d) = (1 + SEGMENTS + i, 1 + SEGMENTS + next);
        mesh.add_triangle(a, c, d);
        mesh.add_triangle(a, d, b);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// How an [`action`] button is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    /// The one filled answer of a screen.
    Primary,
    /// An outlined alternative.
    Secondary,
    /// An outlined alternative in warning red.
    Destructive,
    /// A quiet text button: no fill until hovered.
    Ghost,
}

/// The height of every [`action`] button.
pub const ACTION_HEIGHT: f32 = 36.0;

fn action_font() -> FontId {
    FontId::proportional(14.5)
}

fn action_padding(kind: ActionKind) -> f32 {
    match kind {
        ActionKind::Ghost => 12.0,
        _ => 18.0,
    }
}

/// The width [`action`] takes for `label`: never less than its text, so it never wraps or shrinks.
pub fn action_width(ui: &egui::Ui, label: &str, kind: ActionKind, leading: bool) -> f32 {
    let text = ui
        .painter()
        .layout_no_wrap(label.to_owned(), action_font(), ICE)
        .size()
        .x;
    let width = text + 2.0 * action_padding(kind) + if leading { 14.0 } else { 0.0 };
    match kind {
        ActionKind::Ghost => width,
        _ => width.max(96.0),
    }
}

/// A footer button with animated hover, press and focus. `leading_chevron` draws a "back"
/// chevron before the label. The label never wraps: the button is as wide as its text needs.
/// Muted and not focusable when `enabled` is false (or the `Ui` is disabled).
pub fn action(
    ui: &mut egui::Ui,
    label: &str,
    kind: ActionKind,
    enabled: bool,
    leading_chevron: bool,
) -> Response {
    let enabled = enabled && ui.is_enabled();
    let width = action_width(ui, label, kind, leading_chevron);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(width, ACTION_HEIGHT),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        let time = ui.style().animation_time;
        let lit = enabled && (response.hovered() || response.has_focus());
        let hover = ui
            .ctx()
            .animate_bool_with_time(response.id.with("hover"), lit, time);
        let press = ui.ctx().animate_bool_with_time(
            response.id.with("press"),
            enabled && response.is_pointer_button_down_on(),
            time * 0.5,
        );
        let geometry = ui.style().scroll_animation.duration.max > 0.0;
        let body = rect.shrink(if geometry { press * 0.8 } else { 0.0 });
        let radius = 10.0;
        let painter = ui.painter();
        let (fill, stroke, text) = match (kind, enabled) {
            (ActionKind::Primary, true) => {
                let lift = if geometry { hover * (1.0 - press) } else { 0.0 };
                painter.add(
                    egui::epaint::Shadow {
                        offset: [0, (2.0 + 3.0 * lift) as i8],
                        blur: (10.0 + 10.0 * lift) as u8,
                        spread: 0,
                        color: alpha(FROST, (40.0 + 50.0 * lift) as u8),
                    }
                    .as_shape(body, radius),
                );
                (
                    mix(
                        mix(FROST, GLACIER, hover * 0.55),
                        Color32::from_rgb(8, 151, 192),
                        press,
                    ),
                    Stroke::NONE,
                    MIDNIGHT,
                )
            }
            (ActionKind::Secondary, true) => (
                alpha(NAVY, (70.0 + 70.0 * hover) as u8),
                Stroke::new(1.0, alpha(GLACIER, (80.0 + 90.0 * hover) as u8)),
                ICE,
            ),
            (ActionKind::Destructive, true) => (
                alpha(WARNING, (10.0 + 26.0 * hover) as u8),
                Stroke::new(1.0, alpha(WARNING, (100.0 + 90.0 * hover) as u8)),
                mix(WARNING, ICE, hover * 0.35),
            ),
            (ActionKind::Ghost, true) => (
                alpha(GLACIER, (26.0 * hover + 14.0 * press) as u8),
                Stroke::NONE,
                mix(GLACIER, ICE, hover),
            ),
            (ActionKind::Ghost, false) => (Color32::TRANSPARENT, Stroke::NONE, alpha(QUIET, 110)),
            (_, false) => (
                alpha(NAVY, 45),
                Stroke::new(1.0, alpha(QUIET, 36)),
                alpha(QUIET, 140),
            ),
        };
        painter.rect(body, radius, fill, stroke, StrokeKind::Inside);
        let galley = painter.layout_no_wrap(label.to_owned(), action_font(), text);
        let content = galley.size().x + if leading_chevron { 14.0 } else { 0.0 };
        let left = body.center().x - content * 0.5;
        if leading_chevron {
            let c = Pos2::new(
                left + 3.5 - if geometry { 2.0 * hover } else { 0.0 },
                body.center().y,
            );
            let stroke = Stroke::new(1.6, text);
            painter.line_segment([c + Vec2::new(3.5, -4.5), c], stroke);
            painter.line_segment([c, c + Vec2::new(3.5, 4.5)], stroke);
        }
        let at = Pos2::new(
            left + if leading_chevron { 14.0 } else { 0.0 },
            body.center().y - galley.size().y * 0.5,
        );
        painter.galley(at, galley, text);
        focus_ring(ui, &response, body, radius);
    }
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}
