//! Millimetre geometry and local edits. This module needs no window or egui context.

use std::collections::BTreeSet;

use eframe::egui::{Pos2, Rect, Vec2};

use crate::ctl::{PlaceEntry, Request};
use crate::model::{Status, short_id};

#[derive(Clone, Debug)]
pub struct DisplayRect {
    pub node: String,
    pub machine: String,
    pub display: u32,
    pub name: String,
    pub origin: [f64; 2],
    pub size: [f64; 2],
}

impl DisplayRect {
    pub fn visible(&self) -> bool {
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

fn from_status(status: &Status) -> Vec<DisplayRect> {
    status
        .layout
        .iter()
        .filter_map(|placement| {
            let own = short_id(&status.node);
            let (machine, displays) = if placement.node == own {
                (&status.name, &status.displays)
            } else {
                let peer = status
                    .peers
                    .iter()
                    .find(|peer| short_id(&peer.node) == placement.node)?;
                (&peer.name, &peer.displays)
            };
            let display = displays
                .iter()
                .find(|display| display.id == placement.display);
            if !placement.origin_mm.iter().all(|n| n.is_finite()) {
                return None;
            }
            // Keep placements without known sizes in the group and place request. They are
            // invisible until status supplies their geometry, but move with their machine.
            let size = display.map_or([0.0; 2], |display| display.mm);
            let size = if size.iter().all(|n| n.is_finite() && *n > 0.0) {
                size
            } else {
                [0.0; 2]
            };
            Some(DisplayRect {
                node: placement.node.clone(),
                machine: machine.clone(),
                display: placement.display,
                name: display.map_or_else(String::new, |display| display.name.clone()),
                origin: placement.origin_mm,
                size,
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
pub struct Fit {
    pub scale: f64,
    mm_min: [f64; 2],
    screen_min: Pos2,
}

impl Fit {
    pub fn new(displays: &[DisplayRect], canvas: Rect) -> Self {
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
        let available = canvas.shrink(28.0);
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

    pub fn to_screen(self, mm: [f64; 2]) -> Pos2 {
        self.screen_min
            + Vec2::new(
                ((mm[0] - self.mm_min[0]) * self.scale) as f32,
                ((mm[1] - self.mm_min[1]) * self.scale) as f32,
            )
    }

    pub fn to_mm(self, screen: Pos2) -> [f64; 2] {
        let offset = screen - self.screen_min;
        [
            self.mm_min[0] + f64::from(offset.x) / self.scale,
            self.mm_min[1] + f64::from(offset.y) / self.scale,
        ]
    }

    pub fn rect(self, display: &DisplayRect) -> Rect {
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

#[derive(Debug, Default)]
pub struct Editor {
    pub displays: Vec<DisplayRect>,
    base: Vec<DisplayRect>,
    drag: Option<Drag>,
}

impl Editor {
    pub fn follow(&mut self, status: &Status) {
        if !self.edited() && self.drag.is_none() {
            self.revert(status);
        }
    }

    pub fn revert(&mut self, status: &Status) {
        self.displays = from_status(status);
        self.base.clone_from(&self.displays);
        self.drag = None;
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

    pub fn edited(&self) -> bool {
        !self.moved_nodes().is_empty()
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn start_drag(&mut self, node: &str) {
        self.drag = Some(Drag {
            node: node.into(),
            start: self.displays.clone(),
        });
    }

    /// Delta is always measured from the initial pointer position, not the last snapped frame.
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

    pub fn stop_drag(&mut self) {
        self.drag = None;
    }

    pub fn place_request(&self) -> Request {
        let moved = self.moved_nodes();
        Request::Place {
            placements: self
                .displays
                .iter()
                .filter(|display| moved.contains(display.node.as_str()))
                .map(|display| PlaceEntry {
                    node: display.node.clone(),
                    display: display.display,
                    origin_mm: [display.origin[0].round(), display.origin[1].round()],
                })
                .collect(),
        }
    }

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

pub fn overlap(a: &DisplayRect, b: &DisplayRect) -> bool {
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
    fn place_contains_exactly_moved_group_with_rounded_mm() {
        let mut editor = editor(vec![
            display("a", 0, [0.0, 0.0]),
            display("a", 1, [100.0, 30.0]),
            display("b", 0, [700.0, 0.0]),
        ]);
        editor.start_drag("a");
        editor.drag_by([20.7, -9.2], 1.0);
        editor.stop_drag();
        let Request::Place { placements } = editor.place_request() else {
            panic!("place")
        };
        assert_eq!(
            placements,
            vec![
                PlaceEntry {
                    node: "a".into(),
                    display: 0,
                    origin_mm: [21.0, -9.0]
                },
                PlaceEntry {
                    node: "a".into(),
                    display: 1,
                    origin_mm: [121.0, 21.0]
                }
            ]
        );

        // A placed display whose geometry is absent still travels with its machine.
        let status: Status = serde_json::from_value(serde_json::json!({
            "node":"aaaaaaaaaaaaaaaa000000000000000000000000000000000000000000000000",
            "name":"desktop","displays":[{"id":0,"mm":[100.0,100.0]}],
            "layout":[{"node":"aaaaaaaaaaaaaaaa","display":0,"origin_mm":[0.0,0.0]},
                {"node":"aaaaaaaaaaaaaaaa","display":1,"origin_mm":[100.0,30.0]}]
        }))
        .expect("status");
        editor.revert(&status);
        assert!(editor.displays[0].visible());
        assert!(!editor.displays[1].visible());
        editor.start_drag("aaaaaaaaaaaaaaaa");
        editor.drag_by([20.7, -9.2], 1.0);
        editor.stop_drag();
        let Request::Place { placements } = editor.place_request() else {
            panic!("place")
        };
        assert_eq!(placements.len(), 2);
        assert_eq!(placements[1].origin_mm, [121.0, 21.0]);
    }
}
