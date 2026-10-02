//! Millimetre geometry, the local layout editor and the desk widget.
//!
//! [`Editor`] holds the geometry and the local edits and needs no window or egui context.
//! [`LayoutWidget`] draws the desk and turns pointer input into edits, and reports a pure
//! [`LayoutAction`]. Everything about what a placement *means* stays with the caller: `node` is an
//! opaque key (the kit never shortens, expands or interprets it), the confirmed layout comes from
//! the caller, and a [`PlacementIntent`] is turned into a request, sent and acknowledged by the
//! caller.

use std::collections::BTreeSet;

use egui::{Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};

use crate::crossings::crossings;
use crate::theme;

/// One display of one machine, placed on the desk in millimetres.
///
/// A display whose size is unknown has a zero size: it is invisible (nothing draws, snaps or
/// crosses) but still moves with its machine and still appears in the placement intent.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayRect {
    /// An opaque key identifying the machine, supplied by the caller and returned unchanged in a
    /// [`PlacementIntent`].
    pub node: String,
    /// The machine's name, for labels.
    pub machine: String,
    /// The display's id on its machine.
    pub display: u32,
    /// The display's name, for labels.
    pub name: String,
    /// Top-left corner on the desk, in millimetres.
    pub origin: [f64; 2],
    /// Physical size in millimetres.
    pub size: [f64; 2],
    /// Resolution in pixels. Label data only; geometry is in millimetres. The widget labels a
    /// display with the resolution in [`LayoutView::confirmed`] when it is still there, so the
    /// label stays current during an edit; this value is the fallback.
    pub pixels: [u32; 2],
}

impl DisplayRect {
    /// Only finite, positive sizes are drawn, snapped to and crossed.
    pub(crate) fn visible(&self) -> bool {
        self.size.iter().all(|n| n.is_finite() && *n > 0.0)
    }

    fn edges(&self, axis: usize) -> [f64; 3] {
        let start = self.origin[axis];
        [
            start,
            start + self.size[axis],
            start + self.size[axis] / 2.0,
        ]
    }
}

/// One display's new origin, in whole millimetres. The caller turns this into its own request.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacementIntent {
    /// The [`DisplayRect::node`] key, unchanged.
    pub node: String,
    /// The display's id on its machine.
    pub display: u32,
    /// The new origin in millimetres, already rounded to integers.
    pub origin_mm: [f64; 2],
}

/// What the person asked of the widget this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutAction {
    /// Place these displays: every display of every machine that moved, and no others. The widget
    /// keeps showing the edit until the caller commits (or reverts) it.
    Apply(Vec<PlacementIntent>),
    /// The edits were discarded and the confirmed layout restored. The widget has already done
    /// this; the caller only needs to clear any feedback it was showing.
    Revert,
}

#[derive(Clone, Copy, Debug)]
struct Fit {
    scale: f64,
    mm_min: [f64; 2],
    screen_min: Pos2,
}

impl Fit {
    fn new(displays: &[DisplayRect], canvas: Rect) -> Self {
        let mut min = [f64::INFINITY; 2];
        let mut max = [f64::NEG_INFINITY; 2];
        for display in displays.iter().filter(|display| display.visible()) {
            for axis in 0..2 {
                min[axis] = min[axis].min(display.origin[axis]);
                max[axis] = max[axis].max(display.origin[axis] + display.size[axis]);
            }
        }
        if !min[0].is_finite() {
            min = [0.0; 2];
            max = [1.0; 2];
        }
        // Fit to 85% on the limiting axis, preserving physical proportions and centering.
        // A wide desk necessarily leaves vertical room for moving machines above/below it.
        let available = Rect::from_center_size(canvas.center(), canvas.size() * 0.85);
        let width = (max[0] - min[0]).max(1.0);
        let height = (max[1] - min[1]).max(1.0);
        let scale = (f64::from(available.width().max(1.0)) / width)
            .min(f64::from(available.height().max(1.0)) / height);
        Self {
            scale,
            mm_min: min,
            screen_min: available.center()
                - Vec2::new((width * scale / 2.0) as f32, (height * scale / 2.0) as f32),
        }
    }

    fn to_screen(self, mm: [f64; 2]) -> Pos2 {
        self.screen_min
            + Vec2::new(
                ((mm[0] - self.mm_min[0]) * self.scale) as f32,
                ((mm[1] - self.mm_min[1]) * self.scale) as f32,
            )
    }

    fn to_mm(self, screen: Pos2) -> [f64; 2] {
        let offset = screen - self.screen_min;
        [
            self.mm_min[0] + f64::from(offset.x) / self.scale,
            self.mm_min[1] + f64::from(offset.y) / self.scale,
        ]
    }

    fn rect(self, display: &DisplayRect) -> Rect {
        Rect::from_min_max(
            self.to_screen(display.origin),
            self.to_screen([
                display.origin[0] + display.size[0],
                display.origin[1] + display.size[1],
            ]),
        )
    }
}

#[derive(Clone, Debug)]
struct Drag {
    node: String,
    start: Vec<DisplayRect>,
}

/// The geometry and local edits of a desk layout. It needs no window or egui context.
///
/// Dragging moves every display of the dragged machine together, whether or not its size is known.
#[derive(Clone, Debug, Default)]
pub struct Editor {
    displays: Vec<DisplayRect>,
    base: Vec<DisplayRect>,
    drag: Option<Drag>,
}

impl Editor {
    /// Adopt `confirmed` unless a local edit or a drag is in progress.
    pub fn follow(&mut self, confirmed: &[DisplayRect]) {
        if !self.edited() && self.drag.is_none() {
            self.revert(confirmed);
        }
    }

    /// Discard every edit and any drag, and show `confirmed`.
    pub fn revert(&mut self, confirmed: &[DisplayRect]) {
        self.displays = confirmed.to_vec();
        self.base.clone_from(&self.displays);
        self.drag = None;
    }

    /// The displays as currently edited.
    pub fn displays(&self) -> &[DisplayRect] {
        &self.displays
    }

    fn moved_nodes(&self) -> BTreeSet<&str> {
        self.displays
            .iter()
            .filter(|display| {
                self.base.iter().any(|base| {
                    base.node == display.node
                        && base.display == display.display
                        && base.origin != display.origin
                })
            })
            .map(|display| display.node.as_str())
            .collect()
    }

    /// Whether any display differs from the confirmed layout.
    pub fn edited(&self) -> bool {
        !self.moved_nodes().is_empty()
    }

    /// Whether a drag is in progress.
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Start dragging the machine `node`, remembering where every display is now.
    pub fn start_drag(&mut self, node: &str) {
        self.drag = Some(Drag {
            node: node.into(),
            start: self.displays.clone(),
        });
    }

    /// Move the dragged machine to `delta` millimetres from where the drag started, snapped to
    /// neighbouring edges within eight physical pixels (`pixels_per_mm` is physical pixels per
    /// millimetre, so HiDPI is the caller's multiplication) and rounded to whole millimetres.
    /// The delta is always measured from the start, never from the last snapped frame.
    pub fn drag_by(&mut self, delta: [f64; 2], pixels_per_mm: f64) {
        let Some(drag) = &self.drag else { return };
        let delta = snap_delta(&drag.start, &drag.node, delta, 8.0 / pixels_per_mm);
        for (display, initial) in self.displays.iter_mut().zip(&drag.start) {
            if display.node == drag.node {
                display.origin = [
                    (initial.origin[0] + delta[0]).round(),
                    (initial.origin[1] + delta[1]).round(),
                ];
            }
        }
    }

    /// End the drag, keeping the geometry reached so far as a local edit.
    pub fn stop_drag(&mut self) {
        self.drag = None;
    }

    /// Every display of every machine that moved, with whole-millimetre origins, in display order.
    pub fn place_intent(&self) -> Vec<PlacementIntent> {
        let moved = self.moved_nodes();
        self.displays
            .iter()
            .filter(|display| moved.contains(display.node.as_str()))
            .map(|display| PlacementIntent {
                node: display.node.clone(),
                display: display.display,
                origin_mm: [display.origin[0].round(), display.origin[1].round()],
            })
            .collect()
    }

    /// Indices of displays that overlap another by more than 1 mm (the agent's rule, which also
    /// refuses overlaps between displays of the same machine).
    pub fn overlaps(&self) -> BTreeSet<usize> {
        let mut overlaps = BTreeSet::new();
        for (i, a) in self.displays.iter().enumerate() {
            for (j, b) in self.displays.iter().enumerate().skip(i + 1) {
                if overlap(a, b) {
                    overlaps.insert(i);
                    overlaps.insert(j);
                }
            }
        }
        overlaps
    }
}

fn overlap(a: &DisplayRect, b: &DisplayRect) -> bool {
    (0..2).all(|axis| {
        (a.origin[axis] + a.size[axis]).min(b.origin[axis] + b.size[axis])
            - a.origin[axis].max(b.origin[axis])
            > 1.0
    })
}

/// Align opposing edges first; only then align starts, ends, or centres on the perpendicular
/// axis. Each exposed display edge participates (including stepped, multi-display groups).
fn snap_delta(displays: &[DisplayRect], node: &str, delta: [f64; 2], threshold: f64) -> [f64; 2] {
    let moved: Vec<_> = displays
        .iter()
        .filter(|display| display.node == node && display.visible())
        .collect();
    let neighbours: Vec<_> = displays
        .iter()
        .filter(|display| display.node != node && display.visible())
        .collect();
    let mut flush: [Option<f64>; 2] = [None, None];
    let mut align: [Option<f64>; 2] = [None, None];
    let consider = |best: &mut Option<f64>, correction: f64| {
        if correction.abs() <= threshold && best.is_none_or(|old| correction.abs() < old.abs()) {
            *best = Some(correction);
        }
    };
    for a in &moved {
        for b in &neighbours {
            for axis in 0..2 {
                let other = 1 - axis;
                let ae = a.edges(axis).map(|edge| edge + delta[axis]);
                let be = b.edges(axis);
                let ap = a.edges(other).map(|edge| edge + delta[other]);
                let bp = b.edges(other);
                // Avoid snapping to the extended line of a distant, unrelated display.
                if ap[0] <= bp[1] + threshold && bp[0] <= ap[1] + threshold {
                    for correction in [be[0] - ae[1], be[1] - ae[0]] {
                        consider(&mut flush[axis], correction);
                        if correction.abs() <= threshold {
                            for index in 0..3 {
                                consider(&mut align[other], bp[index] - ap[index]);
                            }
                        }
                    }
                }
            }
        }
    }
    [
        delta[0] + flush[0].or(align[0]).unwrap_or(0.0),
        delta[1] + flush[1].or(align[1]).unwrap_or(0.0),
    ]
}

/// What the widget needs from the caller each frame.
#[derive(Clone, Copy, Debug)]
pub struct LayoutView<'a> {
    /// The current confirmed layout: what Revert restores, and the source of each display's
    /// resolution label (matched by node and display) while an edit keeps older geometry.
    pub confirmed: &'a [DisplayRect],
    /// The [`DisplayRect::node`] of this machine, drawn in Frost cyan.
    pub local_node: &'a str,
    /// The [`DisplayRect::node`] keys of the peers, in the caller's order. Peers alternate between
    /// two tints by their position here; an unknown peer takes the first tint.
    pub peer_order: &'a [String],
    /// A placement is pending or just accepted: edits are frozen until the caller reverts to the
    /// next confirmed layout. Dragging, Apply and Revert are all disabled.
    pub busy: bool,
    /// One line of caller feedback (for example an error) shown under the buttons.
    pub feedback: Option<&'a str>,
}

/// The desk: Apply and Revert, the legend and the draggable canvas.
///
/// The widget owns its [`Editor`] and the canvas-drag transform. The caller keeps the
/// confirmed layout up to date with [`LayoutWidget::follow`] (or [`LayoutWidget::revert`] once a
/// placement is acknowledged), none of which depends on [`LayoutWidget::show`] having run.
#[derive(Debug, Default)]
pub struct LayoutWidget {
    editor: Editor,
    canvas_drag: Option<CanvasDrag>,
}

/// The screen-to-millimetre transform and the pointer position at the start of a drag. They stay
/// fixed for the whole drag, so the desk does not rescale under the pointer while a machine moves.
#[derive(Clone, Copy, Debug)]
struct CanvasDrag {
    fit: Fit,
    pointer_start: Pos2,
}

impl LayoutWidget {
    /// Adopt `confirmed` unless a local edit or a drag is in progress.
    pub fn follow(&mut self, confirmed: &[DisplayRect]) {
        if !self.editor.edited() && !self.editor.dragging() {
            self.revert(confirmed);
        }
    }

    /// Discard every edit and any drag (editor and canvas), and show `confirmed`. Used after a
    /// placement is acknowledged and the next confirmed layout arrives, even while the widget is
    /// not on screen.
    pub fn revert(&mut self, confirmed: &[DisplayRect]) {
        self.editor.revert(confirmed);
        self.canvas_drag = None;
    }

    /// End any drag (editor and canvas), for example when the connection drops or the tab is left.
    /// Nothing is committed: the geometry reached so far stays a local edit.
    pub fn cancel_drag(&mut self) {
        self.editor.stop_drag();
        self.canvas_drag = None;
    }

    /// Draw the desk and handle input. Returns the action the person asked for this frame, if any.
    pub fn show(&mut self, ui: &mut egui::Ui, view: LayoutView<'_>) -> Option<LayoutAction> {
        let busy = view.busy;
        let overlaps = self.editor.overlaps();
        let mut action = None;
        ui.horizontal(|ui| {
            if theme::primary(
                ui,
                "Apply",
                self.editor.edited() && overlaps.is_empty() && !busy && !self.editor.dragging(),
            )
            .clicked()
            {
                action = Some(LayoutAction::Apply(self.editor.place_intent()));
            }
            if ui
                .add_enabled(self.editor.edited() && !busy, egui::Button::new("Revert"))
                .clicked()
            {
                self.revert(view.confirmed);
                action = Some(LayoutAction::Revert);
            }
            if !overlaps.is_empty() {
                ui.colored_label(theme::WARNING, "Displays overlap.");
            }
        });
        if action.is_some() {
            // The caller reacts to the action (feedback text, a request): show that next frame.
            ui.ctx().request_repaint();
        }
        // After a revert the caller clears its feedback, so it is not shown for this frame.
        if let Some(message) = view
            .feedback
            .filter(|_| !matches!(action, Some(LayoutAction::Revert)))
        {
            ui.label(message);
        }
        ui.label(RichText::new("Drag a machine to where it sits on your desk; the pointer crosses where screens touch").color(theme::QUIET));
        ui.horizontal_wrapped(|ui| {
            theme::chip(ui, "●  This machine", theme::FROST);
            theme::chip(ui, "●  Peers", theme::PEER_ICE);
            theme::crossing_legend(ui);
        });
        let (canvas, _) = ui.allocate_exact_size(
            ui.available_size().max(egui::vec2(1.0, 1.0)),
            Sense::hover(),
        );
        let painter = ui.painter_at(canvas);
        theme::gradient(
            &painter,
            canvas,
            12.0,
            theme::alpha(theme::NAVY, 100),
            theme::alpha(theme::MIDNIGHT, 225),
        );
        painter.rect_stroke(
            canvas,
            12.0,
            Stroke::new(1.0, theme::alpha(theme::GLACIER, 45)),
            StrokeKind::Inside,
        );
        // A quiet desk grid establishes scale without competing with the display labels.
        let grid = canvas.shrink(12.0);
        for x in (0..(grid.width() / 32.0) as usize).map(|i| grid.left() + i as f32 * 32.0) {
            for y in (0..(grid.height() / 32.0) as usize).map(|i| grid.top() + i as f32 * 32.0) {
                painter.circle_filled(egui::pos2(x, y), 0.7, theme::alpha(theme::QUIET, 30));
            }
        }
        if !self.editor.displays.iter().any(|display| display.visible()) {
            painter.text(
                canvas.center(),
                egui::Align2::CENTER_CENTER,
                "No placed displays with a known size.",
                FontId::proportional(16.0),
                ui.visuals().text_color(),
            );
            return action;
        }
        let fit = self
            .canvas_drag
            .map_or_else(|| Fit::new(&self.editor.displays, canvas), |drag| drag.fit);
        let mut start = None;
        let mut dragging_node = None;
        for display in self
            .editor
            .displays
            .iter()
            .filter(|display| display.visible())
        {
            let response = ui.interact(
                fit.rect(display),
                ui.id().with((&display.node, display.display)),
                if busy { Sense::hover() } else { Sense::drag() },
            );
            if response.dragged() {
                dragging_node = Some(display.node.clone());
            }
            if response.drag_started() {
                start = ui
                    .input(|input| input.pointer.press_origin())
                    .map(|pointer_start| (display.node.clone(), CanvasDrag { fit, pointer_start }));
            }
            response.on_hover_text(format!(
                "{} / {}\n{:.0} × {:.0} mm at ({:.0}, {:.0}) mm",
                display.machine,
                display.name,
                display.size[0],
                display.size[1],
                display.origin[0],
                display.origin[1]
            ));
        }
        if let Some((node, drag)) = start {
            self.editor.start_drag(&node);
            self.canvas_drag = Some(drag);
        }
        if let Some(drag) = self.canvas_drag {
            if let Some(pointer) = ui.input(|input| input.pointer.interact_pos()) {
                let now = drag.fit.to_mm(pointer);
                let initial = drag.fit.to_mm(drag.pointer_start);
                // Snap threshold is eight physical screen pixels, accounting for HiDPI.
                self.editor.drag_by(
                    [now[0] - initial[0], now[1] - initial[1]],
                    drag.fit.scale * f64::from(ui.ctx().pixels_per_point()),
                );
            }
            if !ui.input(|input| input.pointer.primary_down()) {
                self.editor.stop_drag();
                self.canvas_drag = None;
            }
        }
        let overlaps = self.editor.overlaps();
        for (index, display) in self.editor.displays.iter().enumerate() {
            if !display.visible() {
                continue;
            }
            let rect = fit.rect(display);
            let accent = if display.node == view.local_node {
                theme::FROST
            } else {
                let peer_index = view
                    .peer_order
                    .iter()
                    .position(|peer| *peer == display.node)
                    .unwrap_or(0);
                if peer_index % 2 == 0 {
                    theme::PEER_ICE
                } else {
                    theme::QUIET
                }
            };
            painter.add(
                egui::epaint::Shadow {
                    offset: [0, 4],
                    blur: 12,
                    spread: 0,
                    color: Color32::from_black_alpha(90),
                }
                .as_shape(rect, 9),
            );
            theme::gradient(
                &painter,
                rect,
                9.0,
                if display.node == view.local_node {
                    Color32::from_rgb(20, 64, 96)
                } else {
                    theme::alpha(theme::NAVY, 95)
                },
                if display.node == view.local_node {
                    Color32::from_rgb(9, 31, 51)
                } else {
                    theme::alpha(theme::MIDNIGHT, 170)
                },
            );
            painter.rect_stroke(
                rect,
                9.0,
                Stroke::new(
                    if overlaps.contains(&index) { 2.0 } else { 1.0 },
                    if overlaps.contains(&index) {
                        theme::WARNING
                    } else {
                        theme::alpha(accent, 240)
                    },
                ),
                StrokeKind::Inside,
            );
            let top = [
                rect.left_top() + egui::vec2(10.0, 2.0),
                rect.right_top() + egui::vec2(-10.0, 2.0),
            ];
            for (width, opacity) in [(7.0, 18), (4.0, 35), (2.0, 180)] {
                painter.line_segment(top, Stroke::new(width, theme::alpha(accent, opacity)));
            }
            // The resolution is label data, not geometry: show the latest the caller knows
            // (`confirmed`, matched by node and display) even while an edit or drag keeps the
            // older snapshot of the geometry. A display that has left `confirmed` keeps its own.
            let [width_px, height_px] = view
                .confirmed
                .iter()
                .find(|latest| latest.node == display.node && latest.display == display.display)
                .map_or(display.pixels, |latest| latest.pixels);
            let resolution = format!("{width_px} × {height_px}");
            let text_painter = painter.with_clip_rect(rect.shrink(3.0).intersect(canvas));
            display_machine_chip(&text_painter, rect, &display.machine, accent);
            let name = if display.name.is_empty() {
                format!("Display {}", display.display)
            } else {
                display.name.clone()
            };
            let detail = format!("{resolution} · {}", display.machine);
            for (text, fraction, size, color) in [
                (
                    name,
                    0.52,
                    (rect.height() * 0.18).clamp(8.0, 15.0),
                    theme::ICE,
                ),
                (
                    detail,
                    0.78,
                    (rect.height() * 0.16).clamp(8.0, 11.0),
                    theme::QUIET,
                ),
            ] {
                let width = (rect.width() - 12.0).max(1.0);
                let font = fitted_font(ui, &text, width, size);
                let mut job = egui::text::LayoutJob::simple_singleline(text, font, color);
                job.wrap.max_width = width;
                job.wrap.max_rows = 1;
                job.wrap.break_anywhere = true;
                let label = text_painter.layout_job(job);
                let center = egui::pos2(rect.center().x, rect.top() + rect.height() * fraction);
                text_painter.galley(center - label.size() / 2.0, label, color);
            }
        }
        for crossing in crossings(&self.editor.displays) {
            if crossing
                .displays
                .iter()
                .any(|index| overlaps.contains(index))
            {
                continue;
            }
            let line = [fit.to_screen(crossing.start), fit.to_screen(crossing.end)];
            theme::crossing_glow(&painter, line, true);
        }
        if self.editor.dragging() {
            // Show only actual alignments, after the editor's existing snap and rounding.
            for (i, a) in self
                .editor
                .displays
                .iter()
                .enumerate()
                .filter(|(_, d)| d.visible() && dragging_node.as_deref() == Some(&d.node))
            {
                for b in self
                    .editor
                    .displays
                    .iter()
                    .skip(i + 1)
                    .filter(|b| b.visible() && b.node != a.node)
                {
                    for axis in 0..2 {
                        for ae in [
                            a.origin[axis],
                            a.origin[axis] + a.size[axis] / 2.0,
                            a.origin[axis] + a.size[axis],
                        ] {
                            for be in [
                                b.origin[axis],
                                b.origin[axis] + b.size[axis] / 2.0,
                                b.origin[axis] + b.size[axis],
                            ] {
                                if (ae - be).abs() <= 1.0 {
                                    let p = fit.to_screen(if axis == 0 {
                                        [ae, 0.0]
                                    } else {
                                        [0.0, ae]
                                    });
                                    let line = if axis == 0 {
                                        [
                                            egui::pos2(p.x, canvas.top() + 8.0),
                                            egui::pos2(p.x, canvas.bottom() - 8.0),
                                        ]
                                    } else {
                                        [
                                            egui::pos2(canvas.left() + 8.0, p.y),
                                            egui::pos2(canvas.right() - 8.0, p.y),
                                        ]
                                    };
                                    painter.line_segment(
                                        line,
                                        Stroke::new(1.0, theme::alpha(theme::FROST, 100)),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        action
    }
}

fn fitted_font(ui: &egui::Ui, text: &str, width: f32, desired: f32) -> FontId {
    let measured = ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(text.into(), FontId::proportional(desired), theme::ICE)
            .size()
            .x
    });
    FontId::proportional((desired * (width / measured.max(1.0)).min(1.0)).max(8.0))
}

fn display_machine_chip(painter: &egui::Painter, display: Rect, machine: &str, accent: Color32) {
    let font_size = (display.height() * 0.13).clamp(7.0, 9.0);
    let font = FontId::proportional(font_size);
    let label = painter.layout_no_wrap(machine.into(), font, accent);
    let size = egui::vec2(
        (label.size().x + 12.0).min((display.width() - 16.0).max(1.0)),
        label.size().y + 4.0,
    );
    let rect = Rect::from_min_size(display.min + egui::vec2(8.0, 5.0), size);
    painter.rect_filled(rect, 5.0, theme::alpha(accent, 18));
    painter.rect_stroke(
        rect,
        5.0,
        Stroke::new(1.0, theme::alpha(accent, 70)),
        StrokeKind::Inside,
    );
    painter
        .with_clip_rect(
            rect.shrink2(egui::vec2(4.0, 0.0))
                .intersect(painter.clip_rect()),
        )
        .galley(rect.min + egui::vec2(6.0, 2.0), label, accent);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(node: &str, id: u32, origin: [f64; 2]) -> DisplayRect {
        DisplayRect {
            node: node.into(),
            machine: node.into(),
            display: id,
            name: "screen".into(),
            origin,
            size: [100.0; 2],
            pixels: [1920, 1080],
        }
    }

    fn editor(displays: Vec<DisplayRect>) -> Editor {
        Editor {
            base: displays.clone(),
            displays,
            drag: None,
        }
    }

    #[test]
    fn fit_round_trips_mm_and_screen() {
        let fit = Fit::new(
            &[
                display("a", 0, [-340.0, 20.0]),
                display("b", 0, [0.0, -30.0]),
            ],
            Rect::from_min_size(Pos2::new(10.0, 80.0), Vec2::new(800.0, 480.0)),
        );
        for mm in [[-340.0, 20.0], [100.0, 70.0], [-75.3, 33.7]] {
            let actual = fit.to_mm(fit.to_screen(mm));
            assert!((actual[0] - mm[0]).abs() < 0.001);
            assert!((actual[1] - mm[1]).abs() < 0.001);
        }
        let screen = Pos2::new(333.5, 222.2);
        assert!(fit.to_screen(fit.to_mm(screen)).distance(screen) < 0.001);
    }

    #[test]
    fn fit_uses_eighty_five_percent_of_limiting_axis_and_preserves_aspect() {
        for size in [Vec2::new(760.0, 570.0), Vec2::new(300.0, 500.0)] {
            for origins in [[[0.0, 0.0], [500.0, 0.0]], [[0.0, 0.0], [0.0, 500.0]]] {
                let displays = [display("a", 0, origins[0]), display("b", 0, origins[1])];
                let canvas = Rect::from_min_size(Pos2::new(20.0, 30.0), size);
                let fit = Fit::new(&displays, canvas);
                let bounds = fit.rect(&displays[0]).union(fit.rect(&displays[1]));
                assert!(bounds.center().distance(canvas.center()) < 0.001);
                assert!((bounds.width() / size.x).max(bounds.height() / size.y) <= 0.85001);
                assert!(
                    ((bounds.width() / size.x).max(bounds.height() / size.y) - 0.85).abs() < 0.001
                );
                assert!(
                    (fit.rect(&displays[0]).width() / fit.rect(&displays[0]).height() - 1.0).abs()
                        < 0.001
                );
            }
        }
    }

    #[test]
    fn group_drag_moves_every_display_together() {
        let mut editor = editor(vec![
            display("a", 0, [0.0, 0.0]),
            display("a", 1, [100.0, 30.0]),
            display("b", 0, [700.0, 0.0]),
        ]);
        editor.start_drag("a");
        editor.drag_by([21.0, -9.0], 1.0);
        assert_eq!(editor.displays[0].origin, [21.0, -9.0]);
        assert_eq!(editor.displays[1].origin, [121.0, 21.0]);
        assert_eq!(editor.displays[2].origin, [700.0, 0.0]);
        // Absolute delta, not accumulated from the previous snapped position.
        editor.drag_by([25.0, -10.0], 1.0);
        assert_eq!(editor.displays[1].origin, [125.0, 20.0]);
        editor.stop_drag();
        assert!(editor.edited());
    }

    #[test]
    fn snaps_to_all_neighbour_edges_only_within_eight_pixels() {
        let displays = [display("a", 0, [0.0, 0.0]), display("b", 0, [300.0, 300.0])];
        for (near, expected, far) in [
            ([196.5, 300.0], [200.0, 300.0], [195.5, 300.0]),
            ([403.5, 300.0], [400.0, 300.0], [404.5, 300.0]),
            ([300.0, 196.5], [300.0, 200.0], [300.0, 195.5]),
            ([300.0, 403.5], [300.0, 400.0], [300.0, 404.5]),
        ] {
            // 2 pixels/mm: the threshold is 4 mm, or 8 screen pixels.
            assert_eq!(snap_delta(&displays, "a", near, 8.0 / 2.0), expected);
            assert_eq!(snap_delta(&displays, "a", far, 8.0 / 2.0), far);
        }
        assert_eq!(
            snap_delta(&displays, "a", [196.0, 300.0], 4.0),
            [200.0, 300.0]
        );
    }

    #[test]
    fn snaps_perpendicular_tops_bottoms_and_centres() {
        let mut a = display("a", 0, [0.0, 0.0]);
        a.size[1] = 60.0;
        let displays = [a, display("b", 0, [300.0, 300.0])];
        for (near_y, expected_y) in [(303.0, 300.0), (343.0, 340.0), (323.0, 320.0)] {
            assert_eq!(
                snap_delta(&displays, "a", [197.0, near_y], 4.0),
                [200.0, expected_y]
            );
        }
        assert_eq!(
            snap_delta(&displays, "a", [197.0, 500.0], 4.0),
            [197.0, 500.0]
        );
    }

    #[test]
    fn overlap_uses_agent_one_mm_rule() {
        let a = display("a", 0, [0.0, 0.0]);
        assert!(!overlap(&a, &display("b", 0, [100.0, 0.0])));
        assert!(!overlap(&a, &display("b", 0, [99.0, 0.0])));
        assert!(!overlap(&a, &display("b", 0, [98.0, 99.0])));
        assert!(overlap(&a, &display("b", 0, [98.99, 98.99])));
        // The agent also refuses overlaps between displays of the same machine.
        let editor = editor(vec![
            a,
            display("a", 1, [80.0, 0.0]),
            display("c", 0, [300.0, 0.0]),
        ]);
        assert_eq!(editor.overlaps(), BTreeSet::from([0, 1]));
    }

    #[test]
    fn follow_keeps_edits_and_drags_but_adopts_confirmed_otherwise() {
        let mut widget = LayoutWidget::default();
        let confirmed = vec![display("a", 0, [0.0, 0.0]), display("b", 0, [300.0, 0.0])];
        widget.follow(&confirmed);
        assert_eq!(widget.editor.displays(), confirmed.as_slice());
        // A drag that has not moved anything yet is still protected from `follow`.
        let fit = Fit::new(
            &confirmed,
            Rect::from_min_size(Pos2::ZERO, Vec2::splat(400.0)),
        );
        widget.editor.start_drag("a");
        widget.canvas_drag = Some(CanvasDrag {
            fit,
            pointer_start: Pos2::new(10.0, 10.0),
        });
        let newer = vec![display("a", 0, [50.0, 0.0]), display("b", 0, [300.0, 0.0])];
        widget.follow(&newer);
        assert_eq!(widget.editor.displays(), confirmed.as_slice());
        assert!(widget.editor.dragging() && widget.canvas_drag.is_some());
        // Cancelling ends the drag on both sides and commits nothing; the edit stays local.
        widget.editor.drag_by([20.0, 0.0], 1.0);
        widget.cancel_drag();
        assert!(!widget.editor.dragging() && widget.canvas_drag.is_none());
        assert!(widget.editor.edited());
        widget.follow(&newer);
        assert_eq!(widget.editor.displays()[0].origin, [20.0, 0.0]);
        // A caller-driven revert replaces everything, edit or not.
        widget.revert(&newer);
        assert!(!widget.editor.edited());
        assert_eq!(widget.editor.displays(), newer.as_slice());
    }
}
