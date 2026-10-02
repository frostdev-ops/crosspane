//! Crosspane's dark-only palette, glass surfaces and small shape-based controls.
use eframe::egui::{
    self, Color32, FontId, Pos2, Rect, Response, RichText, Sense, Stroke, StrokeKind, Vec2,
};

pub const MIDNIGHT: Color32 = Color32::from_rgb(0x07, 0x15, 0x25);
pub const NAVY: Color32 = Color32::from_rgb(0x16, 0x4a, 0x74);
pub const FROST: Color32 = Color32::from_rgb(0x17, 0xc8, 0xf4);
pub const GLACIER: Color32 = Color32::from_rgb(0x6f, 0xdc, 0xff);
pub const ICE: Color32 = Color32::from_rgb(0xe9, 0xf8, 0xff);
pub const PEER_ICE: Color32 = Color32::from_rgb(0xb7, 0xef, 0xff);
pub const QUIET: Color32 = Color32::from_rgb(0x89, 0xcb, 0xd5);
pub const WARNING: Color32 = Color32::from_rgb(0xd9, 0x88, 0x91);

pub fn alpha(color: Color32, opacity: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), opacity)
}

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

pub fn destructive(ui: &mut egui::Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(WARNING))
            .stroke(Stroke::new(1.0, alpha(WARNING, 85))),
    )
}

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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn palette_and_dark_theme_construction() {
        assert_eq!(MIDNIGHT.to_array(), [7, 21, 37, 255]);
        assert_eq!(NAVY.to_array(), [22, 74, 116, 255]);
        assert_eq!(FROST.to_array(), [23, 200, 244, 255]);
        assert_eq!(GLACIER.to_array(), [111, 220, 255, 255]);
        assert_eq!(ICE.to_array(), [233, 248, 255, 255]);
        assert_eq!(QUIET.to_array(), [137, 203, 213, 255]);
        let style = style();
        assert!(style.visuals.dark_mode);
        assert_eq!(style.visuals.override_text_color, Some(ICE));
        assert_eq!(style.visuals.selection.stroke.color, GLACIER);
        assert_eq!(style.text_styles[&egui::TextStyle::Heading].size, 24.0);
        assert_eq!(glass().corner_radius, egui::CornerRadius::same(12));
        assert!(glass().fill.a() < 255);
    }
}
