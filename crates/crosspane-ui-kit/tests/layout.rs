//! The layout editor, the contact spans and the desk widget, through the public API only.
//!
//! The editor and crossing tests need no egui context at all. The widget tests run the real
//! widget headless, with synthesized pointer events, on an egui context that has no renderer.
//! Text needs fonts, which the kit never bundles: widget tests load a system font at runtime
//! (`CROSSPANE_TEST_FONT`, or the usual Linux and macOS locations) and skip, saying so, when
//! there is none.
// Helper functions in an integration-test crate are not `#[test]` functions for clippy.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use crosspane_ui_kit::crossings::{Crossing, Edge, crossings};
use crosspane_ui_kit::layout::{
    DisplayRect, Editor, LayoutAction, LayoutView, LayoutWidget, PlacementIntent,
};
use crosspane_ui_kit::theme;
use egui::{Color32, Context, CornerRadius, Event, Pos2, Rect, Shape, Vec2};

fn display(node: &str, id: u32, origin: [f64; 2], size: [f64; 2]) -> DisplayRect {
    DisplayRect {
        node: node.into(),
        machine: format!("{node}-machine"),
        display: id,
        name: format!("{node}{id}"),
        origin,
        size,
        pixels: [1920, 1080],
    }
}

fn square(node: &str, id: u32, origin: [f64; 2]) -> DisplayRect {
    display(node, id, origin, [100.0; 2])
}

fn editor(displays: &[DisplayRect]) -> Editor {
    let mut editor = Editor::default();
    editor.revert(displays);
    editor
}

fn intent(node: &str, display: u32, origin_mm: [f64; 2]) -> PlacementIntent {
    PlacementIntent {
        node: node.into(),
        display,
        origin_mm,
    }
}

// ---- Editor: geometry and edits ---------------------------------------------------------------

#[test]
fn group_drag_moves_every_display_together() {
    let mut editor = editor(&[
        square("a", 0, [0.0, 0.0]),
        square("a", 1, [100.0, 30.0]),
        square("b", 0, [700.0, 0.0]),
    ]);
    assert!(!editor.edited() && !editor.dragging());
    editor.start_drag("a");
    assert!(editor.dragging());
    editor.drag_by([21.0, -9.0], 1.0);
    let displays = editor.displays();
    assert_eq!(displays[0].origin, [21.0, -9.0]);
    assert_eq!(displays[1].origin, [121.0, 21.0]);
    assert_eq!(displays[2].origin, [700.0, 0.0]);
    editor.stop_drag();
    assert!(editor.edited() && !editor.dragging());
}

#[test]
fn drag_delta_is_measured_from_the_drag_start_and_rounded_to_whole_mm() {
    let mut editor = editor(&[square("a", 0, [0.0, 0.0]), square("b", 0, [700.0, 0.0])]);
    editor.start_drag("a");
    editor.drag_by([21.4, -9.6], 1.0);
    assert_eq!(editor.displays()[0].origin, [21.0, -10.0]);
    // Absolute from the start, never accumulated from the previous (rounded, snapped) frame.
    editor.drag_by([25.0, -10.0], 1.0);
    assert_eq!(editor.displays()[0].origin, [25.0, -10.0]);
    editor.drag_by([0.0, 0.0], 1.0);
    assert_eq!(editor.displays()[0].origin, [0.0, 0.0]);
    editor.drag_by([-0.4, 0.5], 1.0);
    assert_eq!(editor.displays()[0].origin, [-0.0, 1.0]);
    // Back at the start means no edit at all.
    editor.drag_by([0.0, 0.0], 1.0);
    editor.stop_drag();
    assert!(!editor.edited());
    // Without a drag, drag_by is inert.
    editor.drag_by([50.0, 50.0], 1.0);
    assert_eq!(editor.displays()[0].origin, [0.0, 0.0]);
}

#[test]
fn snaps_to_neighbour_edges_only_within_eight_physical_pixels() {
    // 2 pixels per millimetre: a 4 mm threshold, which is 8 pixels.
    // Each row: a delta within the threshold and where it ends up (flush), then a delta just
    // outside it and where it ends up (merely rounded).
    for (near, flush, far, rounded) in [
        (
            [196.5, 300.0],
            [200.0, 300.0],
            [195.5, 300.0],
            [196.0, 300.0],
        ),
        (
            [403.5, 300.0],
            [400.0, 300.0],
            [404.5, 300.0],
            [405.0, 300.0],
        ),
        (
            [300.0, 196.5],
            [300.0, 200.0],
            [300.0, 195.5],
            [300.0, 196.0],
        ),
        (
            [300.0, 403.5],
            [300.0, 400.0],
            [300.0, 404.5],
            [300.0, 405.0],
        ),
    ] {
        for (delta, wanted) in [(near, flush), (far, rounded)] {
            let mut editor = editor(&[square("a", 0, [0.0, 0.0]), square("b", 0, [300.0, 300.0])]);
            editor.start_drag("a");
            editor.drag_by(delta, 2.0);
            assert_eq!(editor.displays()[0].origin, wanted, "delta {delta:?}");
        }
    }
}

#[test]
fn snap_threshold_is_physical_so_hidpi_halves_it() {
    // A 3 mm gap to flush contact at x = 200. The pixels-per-millimetre figure is what the
    // widget passes: points per mm times the display's pixels per point.
    for (pixels_per_mm, snaps) in [(2.0, true), (4.0, false), (1.0, true)] {
        let mut editor = editor(&[square("a", 0, [0.0, 0.0]), square("b", 0, [300.0, 300.0])]);
        editor.start_drag("a");
        editor.drag_by([197.0, 300.0], pixels_per_mm);
        let expected = if snaps { 200.0 } else { 197.0 };
        assert_eq!(
            editor.displays()[0].origin,
            [expected, 300.0],
            "{pixels_per_mm} px/mm"
        );
    }
}

#[test]
fn snaps_perpendicular_tops_bottoms_and_centres_after_edge_contact() {
    let mut tall = square("a", 0, [0.0, 0.0]);
    tall.size[1] = 60.0;
    for (near_y, expected_y) in [(303.0, 300.0), (343.0, 340.0), (323.0, 320.0)] {
        let mut editor = editor(&[tall.clone(), square("b", 0, [300.0, 300.0])]);
        editor.start_drag("a");
        editor.drag_by([197.0, near_y], 1.0);
        assert_eq!(editor.displays()[0].origin, [200.0, expected_y]);
    }
    // Far from the neighbour in y, only the edge contact in x snaps.
    let mut editor = editor(&[tall, square("b", 0, [300.0, 300.0])]);
    editor.start_drag("a");
    editor.drag_by([197.0, 500.0], 1.0);
    assert_eq!(editor.displays()[0].origin, [197.0, 500.0]);
}

#[test]
fn unknown_size_displays_move_with_their_machine_but_never_snap_or_overlap() {
    let editor_of = |displays: &[DisplayRect]| editor(displays);
    let mut editor = editor_of(&[
        square("a", 0, [0.0, 0.0]),
        display("a", 1, [100.0, 30.0], [0.0, 0.0]),
        square("b", 0, [700.0, 0.0]),
        display("b", 1, [800.0, 0.0], [f64::NAN, 100.0]),
    ]);
    editor.start_drag("a");
    editor.drag_by([20.7, -9.2], 1.0);
    editor.stop_drag();
    // The invisible display travels with its machine, and appears in the intent.
    assert_eq!(
        editor.place_intent(),
        vec![intent("a", 0, [21.0, -9.0]), intent("a", 1, [121.0, 21.0])]
    );
    // An invisible neighbour is no snap target. With a ghost 3 mm from flush the drag stays
    // where it was put; the same neighbour with a known size snaps to flush contact.
    let mut with_ghost = editor_of(&[
        square("a", 0, [0.0, 0.0]),
        display("b", 0, [203.0, 0.0], [0.0, 0.0]),
    ]);
    with_ghost.start_drag("a");
    with_ghost.drag_by([100.0, 0.0], 1.0);
    assert_eq!(with_ghost.displays()[0].origin, [100.0, 0.0]);
    let mut with_known = editor_of(&[square("a", 0, [0.0, 0.0]), square("b", 0, [203.0, 0.0])]);
    with_known.start_drag("a");
    with_known.drag_by([100.0, 0.0], 1.0);
    assert_eq!(with_known.displays()[0].origin, [103.0, 0.0]);
    // Unknown sizes never overlap anything either.
    let ghost_inside = editor_of(&[
        square("a", 0, [0.0, 0.0]),
        display("b", 0, [10.0, 10.0], [0.0; 2]),
    ]);
    assert!(ghost_inside.overlaps().is_empty());
}

#[test]
fn overlap_uses_the_agents_one_mm_rule_including_the_same_machine() {
    let with_b_at = |b: [f64; 2]| editor(&[square("a", 0, [0.0, 0.0]), square("b", 0, b)]);
    assert!(with_b_at([100.0, 0.0]).overlaps().is_empty());
    assert!(with_b_at([99.0, 0.0]).overlaps().is_empty());
    assert!(with_b_at([98.0, 99.0]).overlaps().is_empty());
    assert_eq!(with_b_at([98.99, 98.99]).overlaps(), BTreeSet::from([0, 1]));
    // The agent also refuses overlaps between displays of the same machine.
    let same_machine = editor(&[
        square("a", 0, [0.0, 0.0]),
        square("a", 1, [80.0, 0.0]),
        square("c", 0, [300.0, 0.0]),
    ]);
    assert_eq!(same_machine.overlaps(), BTreeSet::from([0, 1]));
}

#[test]
fn place_contains_exactly_moved_group_with_rounded_mm() {
    let mut editor = editor(&[
        square("a", 0, [0.0, 0.0]),
        square("a", 1, [100.0, 30.0]),
        square("b", 0, [700.0, 0.0]),
    ]);
    assert!(editor.place_intent().is_empty());
    editor.start_drag("a");
    editor.drag_by([20.7, -9.2], 1.0);
    editor.stop_drag();
    assert_eq!(
        editor.place_intent(),
        vec![intent("a", 0, [21.0, -9.0]), intent("a", 1, [121.0, 21.0])]
    );
    // Moving a second machine adds it, in display order; a machine moved back is dropped.
    editor.start_drag("b");
    editor.drag_by([5.0, 5.0], 1.0);
    editor.stop_drag();
    assert_eq!(
        editor.place_intent(),
        vec![
            intent("a", 0, [21.0, -9.0]),
            intent("a", 1, [121.0, 21.0]),
            intent("b", 0, [705.0, 5.0]),
        ]
    );
    // Moving machine a back to where it was confirmed makes it drop out of the intent again.
    editor.start_drag("a");
    editor.drag_by([-21.0, 9.0], 1.0);
    editor.stop_drag();
    assert_eq!(editor.place_intent(), vec![intent("b", 0, [705.0, 5.0])]);
}

#[test]
fn follow_adopts_confirmed_only_while_unedited_and_revert_always_does() {
    let first = vec![square("a", 0, [0.0, 0.0]), square("b", 0, [300.0, 0.0])];
    let second = vec![square("a", 0, [50.0, 0.0]), square("b", 0, [300.0, 0.0])];
    let mut editor = Editor::default();
    assert!(editor.displays().is_empty());
    editor.follow(&first);
    assert_eq!(editor.displays(), first.as_slice());
    editor.follow(&second);
    assert_eq!(editor.displays(), second.as_slice());
    // A drag that has not moved anything yet already protects the view.
    editor.start_drag("a");
    editor.follow(&first);
    assert_eq!(editor.displays(), second.as_slice());
    editor.drag_by([10.0, 0.0], 1.0);
    editor.stop_drag();
    assert!(editor.edited());
    editor.follow(&first);
    assert_eq!(editor.displays()[0].origin, [60.0, 0.0]);
    // Revert discards the edit and any drag.
    editor.start_drag("b");
    editor.revert(&first);
    assert!(!editor.edited() && !editor.dragging());
    assert_eq!(editor.displays(), first.as_slice());
}

// ---- Crossings: contact spans -----------------------------------------------------------------

#[test]
fn all_opposing_edges_and_partial_spans() {
    let a = square("a", 0, [0.0, 0.0]);
    for (origin, edges, start, end) in [
        (
            [100.0, 25.0],
            [Edge::Right, Edge::Left],
            [100.0, 25.0],
            [100.0, 100.0],
        ),
        (
            [-100.0, 25.0],
            [Edge::Left, Edge::Right],
            [0.0, 25.0],
            [0.0, 100.0],
        ),
        (
            [25.0, 100.0],
            [Edge::Bottom, Edge::Top],
            [25.0, 100.0],
            [100.0, 100.0],
        ),
        (
            [25.0, -100.0],
            [Edge::Top, Edge::Bottom],
            [25.0, 0.0],
            [100.0, 0.0],
        ),
    ] {
        assert_eq!(
            crossings(&[a.clone(), square("b", 0, origin)]),
            vec![Crossing {
                displays: [0, 1],
                edges,
                start,
                end
            }]
        );
    }
}

#[test]
fn no_corner_same_machine_invisible_or_distant_contacts() {
    let a = square("a", 0, [0.0; 2]);
    for b in [
        square("b", 0, [100.0; 2]),
        square("a", 1, [100.0, 0.0]),
        display("b", 0, [0.0; 2], [0.0; 2]),
        display("b", 0, [100.0, 0.0], [f64::INFINITY, 100.0]),
        square("b", 0, [102.01, 0.0]),
        square("b", 0, [50.0; 2]),
    ] {
        assert!(crossings(&[a.clone(), b]).is_empty());
    }
    // Opposing edges within 2 mm touch; the span is painted at their midpoint.
    let touching = crossings(&[a, square("b", 0, [102.0, 0.0])]);
    assert_eq!(touching[0].start, [101.0, 0.0]);
    assert_eq!(touching[0].end, [101.0, 100.0]);
}

#[test]
fn stepped_multi_display_contacts_keep_display_indices() {
    let spans = crossings(&[
        square("a", 0, [0.0; 2]),
        square("a", 1, [0.0, 100.0]),
        square("b", 0, [100.0, 50.0]),
    ]);
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].displays, [0, 2]);
    assert_eq!(spans[1].displays, [1, 2]);
    assert_eq!(spans[0].end, [100.0, 100.0]);
    assert_eq!(spans[1].start, [100.0, 100.0]);
}

// ---- The widget, headless ---------------------------------------------------------------------

fn system_fonts() -> Option<egui::FontDefinitions> {
    let from_env = std::env::var_os("CROSSPANE_TEST_FONT").map(PathBuf::from);
    let known = [
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
    ]
    .map(PathBuf::from);
    for path in from_env.into_iter().chain(known) {
        if let Ok(bytes) = std::fs::read(&path)
            && !bytes.is_empty()
        {
            let mut fonts = egui::FontDefinitions::empty();
            fonts
                .font_data
                .insert("system".into(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.insert(family, vec!["system".into()]);
            }
            return Some(fonts);
        }
    }
    None
}

/// What one frame put on screen that the tests care about.
#[derive(Default)]
struct Seen {
    /// Every text: its string, its screen rectangle and the colour of its first section.
    texts: Vec<(String, Rect, Color32)>,
    /// The border of every display (radius 9) with its stroke colour and width.
    borders: Vec<(Rect, Color32, f32)>,
    /// Crossing glow cores (the opaque Glacier line segments of width 3.5).
    glows: usize,
}

fn collect(shape: &Shape, seen: &mut Seen) {
    match shape {
        Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, seen)),
        Shape::Text(text) => seen.texts.push((
            text.galley.text().to_string(),
            text.galley.rect.translate(text.pos.to_vec2()),
            text.galley.job.sections[0].format.color,
        )),
        Shape::Rect(rect)
            if rect.corner_radius == CornerRadius::same(9) && rect.fill == Color32::TRANSPARENT =>
        {
            seen.borders
                .push((rect.rect, rect.stroke.color, rect.stroke.width));
        }
        Shape::LineSegment { stroke, .. }
            if stroke.width == 3.5 && stroke.color == theme::GLACIER =>
        {
            seen.glows += 1;
        }
        _ => {}
    }
}

/// The legend chip draws one glow swatch of its own.
const LEGEND_GLOWS: usize = 1;

/// A widget on a headless context, driven by synthesized pointer events.
struct Rig {
    ctx: Context,
    widget: LayoutWidget,
    confirmed: Vec<DisplayRect>,
    peers: Vec<String>,
    busy: bool,
    feedback: Option<String>,
    time: f64,
    size: Vec2,
    pointer: Pos2,
    seen: Seen,
}

impl Rig {
    fn new(confirmed: Vec<DisplayRect>, pixels_per_point: f32) -> Option<Self> {
        let Some(fonts) = system_fonts() else {
            eprintln!("skipping: no system font for the widget text");
            return None;
        };
        let ctx = Context::default();
        ctx.set_fonts(fonts);
        ctx.set_theme(egui::Theme::Dark);
        ctx.set_style_of(egui::Theme::Dark, theme::style());
        ctx.set_pixels_per_point(pixels_per_point);
        let mut rig = Self {
            ctx,
            widget: LayoutWidget::default(),
            confirmed,
            peers: vec!["b".into()],
            busy: false,
            feedback: None,
            time: 0.0,
            size: Vec2::new(1100.0, 760.0),
            pointer: Pos2::ZERO,
            seen: Seen::default(),
        };
        rig.widget.follow(&rig.confirmed);
        rig.idle(4);
        Some(rig)
    }

    /// One frame. Returns every action the widget reported (a frame can be several passes).
    fn frame(&mut self, events: Vec<Event>) -> Vec<LayoutAction> {
        self.time += 0.1;
        let mut actions = Vec::new();
        let widget = &mut self.widget;
        let view = LayoutView {
            confirmed: &self.confirmed,
            local_node: "a",
            peer_order: &self.peers,
            busy: self.busy,
            feedback: self.feedback.as_deref(),
        };
        let mut output = self.ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)),
                time: Some(self.time),
                events,
                ..Default::default()
            },
            |ui| actions.extend(widget.show(ui, view)),
        );
        output.textures_delta.clear();
        self.seen = Seen::default();
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut self.seen);
        }
        // What the caller does on Revert: it clears its own feedback.
        if actions.contains(&LayoutAction::Revert) {
            self.feedback = None;
        }
        actions
    }

    fn idle(&mut self, frames: usize) -> Vec<LayoutAction> {
        (0..frames).flat_map(|_| self.frame(Vec::new())).collect()
    }

    fn text(&self, needle: &str) -> Option<&(String, Rect, Color32)> {
        self.seen.texts.iter().find(|(text, _, _)| text == needle)
    }

    fn center_of(&self, needle: &str) -> Pos2 {
        self.text(needle)
            .unwrap_or_else(|| panic!("no text {needle:?} on screen"))
            .1
            .center()
    }

    /// The border rectangle of the display whose name label is `name`.
    fn rect_of(&self, name: &str) -> Rect {
        let at = self.center_of(name);
        self.seen
            .borders
            .iter()
            .filter(|(rect, _, _)| rect.contains(at))
            .map(|(rect, _, _)| *rect)
            .min_by(|a, b| a.area().total_cmp(&b.area()))
            .unwrap_or_else(|| panic!("no display border around {name:?}"))
    }

    fn border_color_of(&self, name: &str) -> Color32 {
        let rect = self.rect_of(name);
        self.seen
            .borders
            .iter()
            .find(|(candidate, _, _)| *candidate == rect)
            .map(|(_, color, _)| *color)
            .expect("border")
    }

    fn apply_enabled(&self) -> bool {
        let color = self.text("Apply").expect("Apply button").2;
        assert!(
            color == theme::MIDNIGHT || color == theme::QUIET,
            "unexpected Apply colour {color:?}"
        );
        color == theme::MIDNIGHT
    }

    fn move_to(&mut self, pos: Pos2) -> Vec<LayoutAction> {
        self.pointer = pos;
        self.frame(vec![Event::PointerMoved(pos)])
    }

    fn button(&mut self, pressed: bool) -> Vec<LayoutAction> {
        let pos = self.pointer;
        self.frame(vec![Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }])
    }

    /// Press on the display `name` and move the pointer by `by` points, leaving it pressed.
    fn drag_hold(&mut self, name: &str, by: Vec2) {
        let from = self.center_of(name);
        self.move_to(from);
        self.idle(2);
        self.button(true);
        for step in 1..=6 {
            self.move_to(from + by * (step as f32 / 6.0));
        }
        self.idle(1);
    }

    fn release(&mut self) {
        self.button(false);
        self.idle(3);
    }

    fn drag(&mut self, name: &str, by: Vec2) {
        self.drag_hold(name, by);
        self.release();
    }

    /// Click the text `label`, returning every action reported by the click and its aftermath.
    fn click(&mut self, label: &str) -> Vec<LayoutAction> {
        let at = self.center_of(label);
        let mut actions = self.move_to(at);
        actions.extend(self.idle(2));
        actions.extend(self.button(true));
        actions.extend(self.button(false));
        actions.extend(self.idle(2));
        actions
    }

    fn points_per_mm(&self, name: &str, size_mm: f64) -> f32 {
        self.rect_of(name).width() / size_mm as f32
    }
}

/// Machine `a` (this one) has two side-by-side displays, peer `b` has one, 300 mm gap between.
fn desk() -> Vec<DisplayRect> {
    vec![
        display("a", 0, [0.0, 0.0], [300.0, 200.0]),
        display("a", 1, [300.0, 0.0], [300.0, 200.0]),
        display("b", 0, [900.0, 0.0], [300.0, 200.0]),
    ]
}

fn by_mm(rig: &Rig, mm: [f64; 2]) -> Vec2 {
    let scale = f64::from(rig.points_per_mm("a0", 300.0));
    Vec2::new((mm[0] * scale) as f32, (mm[1] * scale) as f32)
}

#[test]
fn widget_draws_labels_legend_and_feedback() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    for needle in [
        "Apply",
        "Revert",
        "Drag a machine to where it sits on your desk; the pointer crosses where screens touch",
        "●  This machine",
        "●  Peers",
        "Glowing edges · pointer crossings",
        "a0",
        "1920 × 1080 · a-machine",
        "b0",
        "b-machine",
    ] {
        assert!(rig.text(needle).is_some(), "missing {needle:?}");
    }
    assert!(rig.text("Boom").is_none());
    rig.feedback = Some("Boom".into());
    rig.idle(1);
    assert!(rig.text("Boom").is_some());
    // Nothing to apply or revert on a pristine layout.
    assert!(!rig.apply_enabled());
    assert!(rig.click("Apply").is_empty());
    assert!(rig.click("Revert").is_empty());
}

#[test]
fn machines_are_tinted_local_then_peers_alternating_by_peer_order() {
    let mut displays = desk();
    displays.push(square("c", 0, [0.0, 400.0]));
    displays.push(square("d", 0, [200.0, 400.0]));
    let Some(mut rig) = Rig::new(displays, 1.0) else {
        return;
    };
    rig.peers = vec!["b".into(), "c".into()];
    rig.idle(2);
    let tint = |color: Color32| theme::alpha(color, 240);
    assert_eq!(rig.border_color_of("a0"), tint(theme::FROST));
    assert_eq!(rig.border_color_of("a1"), tint(theme::FROST));
    assert_eq!(rig.border_color_of("b0"), tint(theme::PEER_ICE));
    assert_eq!(rig.border_color_of("c0"), tint(theme::QUIET));
    // A machine missing from the peer order takes the first peer tint.
    assert_eq!(rig.border_color_of("d0"), tint(theme::PEER_ICE));
}

#[test]
fn only_displays_with_a_known_size_draw() {
    let mut displays = desk();
    displays.push(display("b", 1, [1200.0, 0.0], [0.0, 0.0]));
    displays.push(display("b", 2, [1200.0, 0.0], [f64::NAN, 5.0]));
    let Some(rig) = Rig::new(displays, 1.0) else {
        return;
    };
    assert_eq!(rig.seen.borders.len(), 3);
    assert!(rig.text("b1").is_none() && rig.text("b2").is_none());
    // And with none at all, the desk says so and offers nothing to apply.
    let Some(mut empty) = Rig::new(Vec::new(), 1.0) else {
        return;
    };
    assert!(
        empty
            .text("No placed displays with a known size.")
            .is_some()
    );
    assert!(empty.seen.borders.is_empty());
    assert!(empty.click("Apply").is_empty());
}

#[test]
fn dragging_a_machine_moves_it_snaps_flush_and_apply_emits_only_that_machine() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    let before = rig.rect_of("b0");
    // The legend's own swatch is the only glow while no machines touch.
    assert_eq!(rig.seen.glows, LEGEND_GLOWS);
    // 300 mm left would be flush against a1's right edge; stop 9 mm short, inside the threshold.
    rig.drag_hold("b0", by_mm(&rig, [-291.0, 0.0]));
    // While the button is down the edit is in progress and Apply stays off.
    assert!(!rig.apply_enabled());
    rig.release();
    let moved = rig.rect_of("b0");
    assert!(moved.left() < before.left());
    assert!(rig.apply_enabled());
    // Snapped flush against a1: its left edge is at a1's right edge.
    let a1 = rig.rect_of("a1");
    assert!(
        (moved.left() - a1.right()).abs() < 0.6,
        "{moved:?} vs {a1:?}"
    );
    // The touching edge glows, once.
    assert_eq!(rig.seen.glows, LEGEND_GLOWS + 1);
    // The other machine did not move and is not part of the intent.
    assert_eq!(
        rig.click("Apply"),
        vec![LayoutAction::Apply(vec![intent("b", 0, [600.0, 0.0])])]
    );
    // The widget keeps showing the edit until the caller commits or reverts it.
    assert!(rig.apply_enabled());
    assert_eq!(rig.rect_of("b0"), moved);
}

#[test]
fn apply_carries_every_display_of_the_moved_machine_and_nothing_else() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    // Move machine a, with both of its displays, well clear of b: no snapping involved.
    rig.drag("a0", by_mm(&rig, [0.0, 150.0]));
    let actions = rig.click("Apply");
    let [LayoutAction::Apply(intents)] = actions.as_slice() else {
        panic!("expected one Apply, got {actions:?}");
    };
    assert_eq!(intents.len(), 2);
    assert_eq!((intents[0].node.as_str(), intents[0].display), ("a", 0));
    assert_eq!((intents[1].node.as_str(), intents[1].display), ("a", 1));
    for intent in intents {
        // Whole millimetres, same shift for the whole group.
        assert_eq!(intent.origin_mm[0].fract(), 0.0);
        assert_eq!(intent.origin_mm[1].fract(), 0.0);
    }
    assert_eq!(intents[0].origin_mm[0], 0.0);
    assert_eq!(intents[1].origin_mm[0], 300.0);
    assert_eq!(intents[0].origin_mm[1], intents[1].origin_mm[1]);
    assert!((intents[0].origin_mm[1] - 150.0).abs() <= 2.0);
}

#[test]
fn revert_restores_the_confirmed_layout_and_reports_it() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    let original = rig.rect_of("b0");
    rig.drag("b0", by_mm(&rig, [-100.0, 120.0]));
    assert!(rig.apply_enabled());
    assert_ne!(rig.rect_of("b0"), original);
    rig.feedback = Some("Applying…".into());
    rig.idle(1);
    assert!(rig.text("Applying…").is_some());
    assert_eq!(rig.click("Revert"), vec![LayoutAction::Revert]);
    assert_eq!(rig.rect_of("b0"), original);
    assert!(!rig.apply_enabled());
    assert!(rig.click("Revert").is_empty());
    assert!(rig.click("Apply").is_empty());
}

#[test]
fn overlap_warns_and_blocks_apply_but_not_revert() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    assert!(rig.text("Displays overlap.").is_none());
    // Drop b on top of a1.
    rig.drag("b0", by_mm(&rig, [-450.0, 40.0]));
    assert!(rig.text("Displays overlap.").is_some());
    assert!(!rig.apply_enabled());
    assert!(rig.click("Apply").is_empty());
    // Overlapping displays are not a crossing: no glow for them.
    assert_eq!(rig.seen.glows, LEGEND_GLOWS);
    assert_eq!(rig.click("Revert"), vec![LayoutAction::Revert]);
    assert!(rig.text("Displays overlap.").is_none());
}

#[test]
fn busy_freezes_dragging_apply_and_revert() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    rig.drag("b0", by_mm(&rig, [-100.0, 120.0]));
    let edited = rig.rect_of("b0");
    assert!(rig.apply_enabled());
    rig.busy = true;
    rig.idle(2);
    assert!(!rig.apply_enabled());
    assert!(rig.click("Apply").is_empty());
    assert!(rig.click("Revert").is_empty());
    // A busy desk cannot be dragged either.
    rig.drag("a0", by_mm(&rig, [0.0, 150.0]));
    assert_eq!(rig.rect_of("b0"), edited);
    assert_eq!(rig.rect_of("a0").top(), rig.rect_of("a1").top());
    // Once the caller reverts to the next confirmed layout, the edit is gone and the desk lives.
    rig.busy = false;
    rig.widget.revert(&desk());
    rig.idle(2);
    assert!(!rig.apply_enabled());
    assert_ne!(rig.rect_of("b0"), edited);
}

#[test]
fn hidpi_halves_the_snap_threshold_in_millimetres() {
    // Drag b to 9 mm short of flush. At 1 pixel per point the 8-pixel threshold is wide enough
    // to snap; at 2 pixels per point it is half as many millimetres and does not.
    let mut results = Vec::new();
    for pixels_per_point in [1.0, 2.0] {
        let Some(mut rig) = Rig::new(desk(), pixels_per_point) else {
            return;
        };
        let scale = f64::from(rig.points_per_mm("a0", 300.0));
        // The widget fits the desk to the canvas: check the premise of the numbers above.
        let threshold_mm = 8.0 / (scale * f64::from(pixels_per_point));
        assert!(
            (threshold_mm > 9.0) == (pixels_per_point == 1.0),
            "scale {scale} makes the premise false"
        );
        rig.drag("b0", by_mm(&rig, [-291.0, 0.0]));
        let actions = rig.click("Apply");
        let [LayoutAction::Apply(intents)] = actions.as_slice() else {
            panic!("expected one Apply, got {actions:?}");
        };
        results.push(intents[0].origin_mm[0]);
    }
    assert_eq!(results, vec![600.0, 609.0]);
}

#[test]
fn the_drag_start_transform_stays_fixed_for_the_whole_drag() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    let anchor = rig.rect_of("a0");
    // Drag b far to the right: the desk's bounds grow, but the picture must not rescale under
    // the pointer while the button is down.
    rig.drag_hold("b0", by_mm(&rig, [500.0, 0.0]));
    assert_eq!(rig.rect_of("a0"), anchor);
    rig.idle(3);
    assert_eq!(rig.rect_of("a0"), anchor);
    // After the release the desk is fitted afresh to the new, larger extent.
    rig.release();
    assert!(rig.rect_of("a0").width() < anchor.width());
}

#[test]
fn caller_driven_revert_clears_the_edit_and_the_canvas_drag_even_offscreen() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    let mut next = desk();
    next[2].origin = [700.0, 100.0];
    let next_rect = {
        // What the desk looks like with `next` confirmed and nothing in progress.
        let Some(mut fresh) = Rig::new(next.clone(), 1.0) else {
            return;
        };
        fresh.idle(2);
        fresh.rect_of("b0")
    };
    rig.drag_hold("b0", by_mm(&rig, [-100.0, 50.0]));
    // The caller learns the placement was committed while the pointer is still down, with the
    // desk not even shown for a while: it reverts the widget directly.
    rig.widget.revert(&next);
    rig.confirmed = next;
    rig.idle(2);
    assert!(!rig.apply_enabled());
    assert_eq!(rig.rect_of("b0"), next_rect);
    // The pointer is still down and still moving: nothing follows it any more.
    let from = rig.pointer;
    for step in 1..=4 {
        rig.move_to(from + Vec2::new(-20.0 * step as f32, 10.0 * step as f32));
    }
    assert_eq!(rig.rect_of("b0"), next_rect);
    rig.release();
    assert_eq!(rig.rect_of("b0"), next_rect);
    assert!(!rig.apply_enabled());
}

#[test]
fn cancel_drag_ends_the_drag_without_committing_or_discarding_the_edit() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    let anchor = rig.rect_of("a0");
    rig.drag_hold("b0", by_mm(&rig, [-100.0, 150.0]));
    assert_eq!(
        rig.rect_of("a0"),
        anchor,
        "the drag-start transform is held"
    );
    // The connection drops (or the tab is left) mid-drag.
    rig.widget.cancel_drag();
    rig.idle(1);
    assert_ne!(
        rig.rect_of("a0"),
        anchor,
        "the desk is fitted afresh once the drag-start transform is dropped"
    );
    let at_cancel = rig.rect_of("b0");
    // The pointer is still down and moving: the display no longer follows it.
    let from = rig.pointer;
    for step in 1..=4 {
        rig.move_to(from + Vec2::new(-30.0 * step as f32, 0.0));
    }
    assert_eq!(rig.rect_of("b0"), at_cancel);
    assert!(rig.idle(2).is_empty(), "cancelling reports no action");
    rig.release();
    // Nothing was committed on its own, and the local edit is still there to apply or revert.
    assert_eq!(rig.rect_of("b0"), at_cancel);
    assert!(rig.apply_enabled());
    // A later confirmed layout does not wipe the edit.
    rig.widget.follow(&desk());
    rig.idle(2);
    assert_eq!(rig.rect_of("b0"), at_cancel);
    let actions = rig.click("Apply");
    assert!(matches!(actions.as_slice(), [LayoutAction::Apply(intents)] if intents.len() == 1));
}

#[test]
fn follow_adopts_confirmed_while_unedited_and_keeps_edits_and_drags() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    let mut next = desk();
    next[2].origin = [700.0, 100.0];
    // Unedited: follow adopts the new confirmed layout, even if the widget is never shown.
    let before = rig.rect_of("b0");
    rig.widget.follow(&next);
    rig.confirmed = next.clone();
    rig.idle(2);
    assert_ne!(rig.rect_of("b0"), before);
    assert!(!rig.apply_enabled());
    // Edited: follow keeps the edit exactly.
    rig.drag("a0", by_mm(&rig, [0.0, 150.0]));
    let edited = rig.rect_of("a0");
    rig.widget.follow(&desk());
    rig.idle(2);
    assert_eq!(rig.rect_of("a0"), edited);
    assert!(rig.apply_enabled());
    // Mid-drag: follow keeps the drag going.
    rig.drag_hold("b0", by_mm(&rig, [0.0, 40.0]));
    let dragged = rig.rect_of("b0");
    rig.widget.follow(&desk());
    rig.idle(1);
    assert_eq!(rig.rect_of("b0"), dragged);
    rig.move_to(rig.pointer + Vec2::new(0.0, 10.0));
    assert_ne!(rig.rect_of("b0"), dragged, "the drag continues");
}

#[test]
fn resolution_labels_follow_the_latest_status_while_the_edit_keeps_its_geometry() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    let label_a = |pixels: &str| format!("{pixels} · a-machine");
    let label_b = |pixels: &str| format!("{pixels} · b-machine");
    assert!(rig.text(&label_b("1920 × 1080")).is_some());
    // An edit is in progress (finished, not applied)...
    rig.drag("b0", by_mm(&rig, [-100.0, 120.0]));
    let edited_b = rig.rect_of("b0");
    let (a0, a1) = (rig.rect_of("a0"), rig.rect_of("a1"));
    // ...when a status arrives with new resolutions (and, to prove the edit wins, new geometry
    // for the edited machine). `follow` keeps the edit exactly, as it always did.
    let mut latest = desk();
    latest[0].pixels = [3840, 2160];
    latest[2].pixels = [2560, 1440];
    latest[2].origin = [1000.0, 50.0];
    rig.widget.follow(&latest);
    rig.confirmed = latest.clone();
    rig.idle(2);
    assert!(rig.text(&label_b("2560 × 1440")).is_some());
    assert!(rig.text(&label_b("1920 × 1080")).is_none());
    assert!(rig.text(&label_a("3840 × 2160")).is_some());
    // The display that did not change keeps its label; nothing moved.
    assert!(rig.text(&label_a("1920 × 1080")).is_some());
    assert_eq!(rig.rect_of("b0"), edited_b);
    assert_eq!((rig.rect_of("a0"), rig.rect_of("a1")), (a0, a1));
    assert!(rig.apply_enabled());
    // A display that has left the confirmed layout keeps the snapshot it was edited with.
    rig.confirmed = latest[..2].to_vec();
    rig.idle(2);
    assert!(rig.text(&label_b("1920 × 1080")).is_some());
    assert!(rig.text(&label_b("2560 × 1440")).is_none());
    assert_eq!(rig.rect_of("b0"), edited_b);
}

#[test]
fn resolution_labels_follow_the_latest_status_during_a_drag_too() {
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    rig.drag_hold("b0", by_mm(&rig, [-100.0, 120.0]));
    let dragged = rig.rect_of("b0");
    let mut latest = desk();
    latest[2].pixels = [2560, 1440];
    rig.widget.follow(&latest);
    rig.confirmed = latest;
    rig.idle(1);
    assert!(rig.text("2560 × 1440 · b-machine").is_some());
    assert_eq!(rig.rect_of("b0"), dragged);
    // The drag carries on from where it was.
    rig.move_to(rig.pointer + Vec2::new(0.0, 10.0));
    assert_ne!(rig.rect_of("b0"), dragged);
    assert!(rig.text("2560 × 1440 · b-machine").is_some());
}

#[test]
fn hover_text_and_ids_do_not_depend_on_the_peer_order_length() {
    // Peers missing from the order, or an empty order, must not break drawing.
    let Some(mut rig) = Rig::new(desk(), 1.0) else {
        return;
    };
    rig.peers.clear();
    rig.idle(2);
    assert_eq!(rig.seen.borders.len(), 3);
    assert_eq!(
        rig.border_color_of("b0"),
        theme::alpha(theme::PEER_ICE, 240)
    );
}
