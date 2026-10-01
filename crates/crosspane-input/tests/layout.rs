#![allow(clippy::unwrap_used)]

use crosspane_input::layout::{
    Layout, LayoutError, LayoutOptions, MIN_PORTAL_MM, Placed, PointerTracker, Step,
    TOUCH_TOLERANCE_MM,
};
use crosspane_input::{Edge, PortalId};
use crosspane_platform::CapturePortal;
use crosspane_types::geom::{
    DisplayGeometry, PixelSize, PointDevice, PointLogical, PointMm, SizeMm, VectorMm,
};
use crosspane_types::id::{DisplayId, GlobalDisplayId, NodeId};
use proptest::prelude::*;

const EPSILON: f64 = 1e-8;

fn id(node: u8, display: u32) -> GlobalDisplayId {
    GlobalDisplayId {
        node: NodeId([node; 32]),
        display: DisplayId(display),
    }
}

fn panel(node: u8, display: u32, x: f64, y: f64, width: f64, height: f64) -> Placed {
    Placed {
        id: id(node, display),
        origin: PointMm::new(x, y),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(width, height),
            pixel_size: PixelSize::new(1_000, 1_000),
            scale: 1.0,
            logical_origin: PointLogical::zero(),
        },
    }
}

fn layout(displays: Vec<Placed>) -> Layout {
    Layout::new(displays, LayoutOptions::default()).unwrap()
}

fn vertical(edge: Edge) -> bool {
    matches!(edge, Edge::Left | Edge::Right)
}

fn opposite(edge: Edge) -> Edge {
    match edge {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
    }
}

fn canvas_span(
    layout: &Layout,
    from: GlobalDisplayId,
    edge: Edge,
    start: f64,
    end: f64,
) -> (f64, f64) {
    let p = if vertical(edge) {
        layout
            .to_canvas(from, PointDevice::new(0.0, start))
            .unwrap()
    } else {
        layout
            .to_canvas(from, PointDevice::new(start, 0.0))
            .unwrap()
    };
    let q = if vertical(edge) {
        layout.to_canvas(from, PointDevice::new(0.0, end)).unwrap()
    } else {
        layout.to_canvas(from, PointDevice::new(end, 0.0)).unwrap()
    };
    if vertical(edge) {
        (p.y, q.y)
    } else {
        (p.x, q.x)
    }
}

fn inside(display: &Placed, position: PointDevice) -> bool {
    position.x >= 0.0
        && position.x <= f64::from(display.geometry.pixel_size.width - 1)
        && position.y >= 0.0
        && position.y <= f64::from(display.geometry.pixel_size.height - 1)
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn near(a: f64, b: f64) {
    assert!(close(a, b), "{a} != {b}");
}

fn positions_close(a: PointDevice, b: PointDevice) -> bool {
    close(a.x, b.x) && close(a.y, b.y)
}

fn assert_position(
    actual: (GlobalDisplayId, PointDevice),
    expected: (GlobalDisplayId, PointDevice),
) {
    assert_eq!(actual.0, expected.0);
    near(actual.1.x, expected.1.x);
    near(actual.1.y, expected.1.y);
}

fn assert_on(step: Step, display: GlobalDisplayId, position: PointDevice) {
    let Step::On {
        display: actual,
        position: point,
    } = step
    else {
        panic!("expected On, got {step:?}");
    };
    assert_position((actual, point), (display, position));
}

// Each row is a node with 1–3 displays. Rows have independent offsets, sizes and gaps;
// transposition also exercises layouts with nodes side by side. Sizes/densities vary by display.
fn random_layout() -> impl Strategy<Value = Layout> {
    let cell = (
        40.0_f64..250.0,
        40.0_f64..180.0,
        640_u32..4_096,
        480_u32..2_160,
        1.0_f64..3.0,
        prop_oneof![Just(0.0), Just(2.0), Just(5.0)],
    );
    let row = (
        -50.0_f64..50.0,
        prop_oneof![Just(0.0), Just(2.0), Just(5.0)],
        prop::collection::vec(cell, 1..=3),
    );
    (
        prop::collection::vec(row, 2..=4),
        any::<bool>(),
        0.0_f64..8.0,
    )
        .prop_map(|(rows, transpose, dead_corner_mm)| {
            let mut displays = Vec::new();
            let mut y = -250.0;
            for (row_index, (offset, row_gap, cells)) in rows.into_iter().enumerate() {
                let height = cells.iter().map(|cell| cell.1).fold(0.0, f64::max);
                let mut x = offset;
                for (display_index, (width, height, px, py, scale, gap)) in
                    cells.into_iter().enumerate()
                {
                    let mut display = Placed {
                        id: id(row_index as u8 + 1, display_index as u32 + 1),
                        origin: PointMm::new(x, y),
                        geometry: DisplayGeometry {
                            physical_size: SizeMm::new(width, height),
                            pixel_size: PixelSize::new(px, py),
                            scale,
                            logical_origin: PointLogical::new(-500.0, 1_000.0),
                        },
                    };
                    if transpose {
                        display.origin = PointMm::new(y, x);
                        display.geometry.physical_size = SizeMm::new(height, width);
                        display.geometry.pixel_size = PixelSize::new(py, px);
                    }
                    displays.push(display);
                    x += width + gap;
                }
                y += height + row_gap;
            }
            Layout::new(displays, LayoutOptions { dead_corner_mm }).unwrap()
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 1_024,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn portals_come_in_pairs(layout in random_layout()) {
        for portal in layout.portals() {
            prop_assert_ne!(portal.from.node, portal.to.node);
            let span = canvas_span(&layout, portal.from, portal.edge, portal.start, portal.end);
            let reverse: Vec<_> = layout.portals().iter().filter(|candidate| {
                candidate.from == portal.to && candidate.to == portal.from
                    && candidate.edge == opposite(portal.edge)
            }).collect();
            prop_assert_eq!(reverse.len(), 1);
            let back = canvas_span(&layout, reverse[0].from, reverse[0].edge, reverse[0].start, reverse[0].end);
            prop_assert!((span.0 - back.0).abs() < EPSILON);
            prop_assert!((span.1 - back.1).abs() < EPSILON);
        }
    }

    #[test]
    fn portal_stretches_fit_edges(layout in random_layout()) {
        for portal in layout.portals() {
            let display = layout.get(portal.from).unwrap();
            let length = if vertical(portal.edge) {
                display.geometry.pixel_size.height
            } else {
                display.geometry.pixel_size.width
            };
            prop_assert!(portal.start >= 0.0 && portal.start < portal.end);
            prop_assert!(portal.end <= f64::from(length));
            let (start, end) = canvas_span(&layout, portal.from, portal.edge, portal.start, portal.end);
            prop_assert!(end - start >= MIN_PORTAL_MM - EPSILON);
        }
    }

    #[test]
    fn entry_is_inside_at_same_canvas_coordinate(layout in random_layout(), fraction in -1.0_f64..2.0) {
        for portal in layout.portals() {
            let span = canvas_span(&layout, portal.from, portal.edge, portal.start, portal.end);
            for fraction in [fraction, 0.0, 0.5, 1.0] {
                let (target, position) = layout.entry(portal.id, fraction).unwrap();
                prop_assert_eq!(target, portal.to);
                let display = layout.get(target).unwrap();
                prop_assert!(inside(display, position));
                let distance = match portal.edge {
                    Edge::Left => f64::from(display.geometry.pixel_size.width) - position.x,
                    Edge::Right => position.x,
                    Edge::Top => f64::from(display.geometry.pixel_size.height) - position.y,
                    Edge::Bottom => position.y,
                };
                prop_assert!((0.0..=1.0 + EPSILON).contains(&distance));
                let canvas = layout.to_canvas(target, position).unwrap();
                let coordinate = if vertical(portal.edge) { canvas.y } else { canvas.x };
                let exit = span.0 + (span.1 - span.0) * fraction.clamp(0.0, 1.0);
                prop_assert!((coordinate - exit).abs() <= 0.5);
                prop_assert_eq!(layout.locate(canvas).unwrap().0, target);
            }
        }
    }

    #[test]
    fn canvas_locate_round_trip(layout in random_layout(), x in 0.0_f64..1.0, y in 0.0_f64..1.0) {
        for display in layout.displays() {
            let point = PointDevice::new(
                x * f64::from(display.geometry.pixel_size.width),
                y * f64::from(display.geometry.pixel_size.height),
            );
            let canvas = layout.to_canvas(display.id, point).unwrap();
            let (found, result) = layout.locate(canvas).unwrap();
            prop_assert_eq!(found, display.id);
            prop_assert!((result.x - point.x).abs() < EPSILON);
            prop_assert!((result.y - point.y).abs() < EPSILON);
        }
    }

    #[test]
    fn tracker_stays_on_displays_and_crosses_only_through_portals(
        layout in random_layout(),
        start in any::<usize>(),
        steps in prop::collection::vec((-3_000.0_f64..3_000.0, -3_000.0_f64..3_000.0), 1..100),
    ) {
        let display = &layout.displays()[start % layout.displays().len()];
        let mut tracker = PointerTracker::new(&layout, display.id, PointDevice::new(100.0, 100.0)).unwrap();
        for (dx, dy) in steps {
            let previous_node = tracker.position().0.node;
            let step = tracker.step(&layout, VectorMm::new(dx, dy));
            let (target, position) = tracker.position();
            prop_assert!(inside(layout.get(target).unwrap(), position));
            prop_assert_eq!(layout.locate(layout.to_canvas(target, position).unwrap()).unwrap().0, target);
            match step {
                Step::On { display, position: result } => {
                    prop_assert_eq!(display.node, previous_node);
                    prop_assert_eq!(display, target);
                    prop_assert!(positions_close(result, position));
                }
                Step::Crossed { portal, display, position: result } => {
                    let portal = layout.portals().iter().find(|candidate| candidate.id == portal).unwrap();
                    prop_assert_eq!(portal.from.node, previous_node);
                    prop_assert_ne!(portal.to.node, previous_node);
                    prop_assert_eq!(portal.to, target);
                    prop_assert_eq!(display, target);
                    prop_assert!(positions_close(result, position));
                }
            }
        }
    }

    #[test]
    fn steps_toward_unconnected_edges_clamp_and_slide(layout in random_layout(), tangent in -200.0_f64..200.0) {
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            let display = layout.displays().iter().min_by(|a, b| {
                let key = |display: &Placed| match edge {
                    Edge::Left => display.rect().min().x,
                    Edge::Right => -display.rect().max().x,
                    Edge::Top => display.rect().min().y,
                    Edge::Bottom => -display.rect().max().y,
                };
                key(a).total_cmp(&key(b))
            }).unwrap();
            let initial = PointDevice::new(
                f64::from(display.geometry.pixel_size.width) / 2.0,
                f64::from(display.geometry.pixel_size.height) / 2.0,
            );
            let delta = match edge {
                Edge::Left => VectorMm::new(-1e6, tangent),
                Edge::Right => VectorMm::new(1e6, tangent),
                Edge::Top => VectorMm::new(tangent, -1e6),
                Edge::Bottom => VectorMm::new(tangent, 1e6),
            };
            let mut tracker = PointerTracker::new(&layout, display.id, initial).unwrap();
            let step = tracker.step(&layout, delta);
            // Tangential motion can reach a neighbour, so restrict this check to a slide that
            // remains within the original edge's tangent extent.
            let local = display.geometry.device_to_mm(initial) + delta;
            let tangent_inside = if vertical(edge) {
                local.y > 0.0 && local.y < display.geometry.physical_size.height
            } else {
                local.x > 0.0 && local.x < display.geometry.physical_size.width
            };
            if tangent_inside {
                prop_assert!(matches!(step, Step::On { .. }), "unconnected edge crossed");
                prop_assert_eq!(tracker.position().0, display.id);
                let expected = display.geometry.clamp_device(display.geometry.mm_to_device(local));
                let position = tracker.position().1;
                prop_assert!((position.x - expected.x).abs() < EPSILON);
                prop_assert!((position.y - expected.y).abs() < EPSILON);
            }
        }
    }
}

#[test]
fn unequal_side_by_side_panels() {
    let mut desktop = panel(1, 1, -800.0, 0.0, 800.0, 335.0);
    desktop.geometry.pixel_size = PixelSize::new(3_440, 1_440);
    let mut retina = panel(2, 1, 0.0, 60.0, 302.0, 196.0);
    retina.geometry.pixel_size = PixelSize::new(3_024, 1_964);
    retina.geometry.scale = 2.0;
    let layout = layout(vec![desktop, retina]);
    assert_eq!(layout.displays(), &[desktop, retina]);
    assert_eq!(layout.portals().len(), 2);
    let portal = layout.portals()[0];
    assert_eq!(
        (portal.from, portal.to, portal.edge),
        (desktop.id, retina.id, Edge::Right)
    );
    near(portal.start, 60.0 * 1_440.0 / 335.0);
    near(portal.end, 256.0 * 1_440.0 / 335.0);
    let capture = layout.capture_portals(desktop.id.node);
    assert_eq!(
        capture,
        vec![CapturePortal {
            id: portal.id,
            display: desktop.id.display,
            edge: Edge::Right,
            from: portal.start,
            to: portal.end,
        }]
    );
    assert!(layout.capture_portals(NodeId([99; 32])).is_empty());
    let (target, entry) = layout.entry(portal.id, 0.5).unwrap();
    assert_eq!(target, retina.id);
    near(entry.x, 0.0);
    near(entry.y, 98.0 * 1_964.0 / 196.0);
    for (input, clamped) in [
        (-10.0, 0.0),
        (10.0, 1.0),
        (f64::NAN, 0.0),
        (f64::NEG_INFINITY, 0.0),
        (f64::INFINITY, 1.0),
    ] {
        assert_position(
            layout.entry(portal.id, input).unwrap(),
            layout.entry(portal.id, clamped).unwrap(),
        );
    }
    assert!(layout.entry(PortalId(99), 0.0).is_none());
    assert!(layout.get(id(99, 1)).is_none());
    assert!(layout.to_canvas(id(99, 1), PointDevice::zero()).is_none());
    assert!(PointerTracker::new(&layout, id(99, 1), PointDevice::zero()).is_none());
    assert!(layout.locate(PointMm::new(-900.0, 100.0)).is_none());
    assert!(layout.locate(PointMm::new(302.0, 100.0)).is_none());
    assert_position(
        layout.locate(retina.origin).unwrap(),
        (retina.id, PointDevice::zero()),
    );

    // A long diagonal step intersects the first portal at y=110 mm, rather than its endpoint.
    let start = desktop.geometry.mm_to_device(PointMm::new(700.0, 100.0));
    let mut tracker = PointerTracker::new(&layout, desktop.id, start).unwrap();
    let Step::Crossed {
        portal: crossed,
        display,
        position,
    } = tracker.step(&layout, VectorMm::new(2_000.0, 200.0))
    else {
        panic!("long step missed the first portal");
    };
    assert_eq!((crossed, display), (portal.id, retina.id));
    near(position.x, 0.0);
    near(position.y, 50.0 * 1_964.0 / 196.0);
    assert_position(tracker.position(), (display, position));
    assert_position(
        layout.entry(portal.id, (110.0 - 60.0) / 196.0).unwrap(),
        (display, position),
    );
}

#[test]
fn stacked_panels() {
    let top = panel(1, 1, 0.0, 0.0, 200.0, 100.0);
    let bottom = panel(2, 1, 50.0, 100.0, 100.0, 150.0);
    let layout = layout(vec![top, bottom]);
    assert_eq!(layout.portals().len(), 2);
    assert_eq!(layout.portals()[0].edge, Edge::Bottom);
    assert_eq!(layout.portals()[1].edge, Edge::Top);
    near(layout.portals()[0].start, 250.0);
    near(layout.portals()[0].end, 750.0);
    let mut tracker = PointerTracker::new(&layout, top.id, PointDevice::new(500.0, 500.0)).unwrap();
    assert!(
        matches!(tracker.step(&layout, VectorMm::new(0.0, 1_000.0)), Step::Crossed { display, .. } if display == bottom.id)
    );
    // Move inward first: the entry edge now rejects immediate reverse jitter.
    assert!(matches!(
        tracker.step(&layout, VectorMm::new(0.0, 2.0)),
        Step::On { .. }
    ));
    assert!(
        matches!(tracker.step(&layout, VectorMm::new(0.0, -1_000.0)), Step::Crossed { display, .. } if display == top.id)
    );
    near(tracker.position().1.y, 999.0);
    // Right/Bottom are exclusive, and an exactly shared edge belongs to the neighbour.
    assert_eq!(
        layout.locate(PointMm::new(100.0, 100.0)).unwrap().0,
        bottom.id
    );
    assert!(layout.locate(PointMm::new(200.0, 50.0)).is_none());
}

#[test]
fn same_node_continues_across_displays() {
    let a = panel(1, 1, 0.0, 0.0, 100.0, 100.0);
    let b = panel(1, 2, 100.0, 0.0, 100.0, 100.0);
    let c = panel(1, 3, 200.0, 0.0, 100.0, 100.0);
    let layout = layout(vec![a, b, c]);
    assert!(layout.portals().is_empty());
    let mut tracker = PointerTracker::new(&layout, a.id, PointDevice::new(500.0, 500.0)).unwrap();
    assert_on(
        tracker.step(&layout, VectorMm::new(175.0, 10.0)),
        c.id,
        PointDevice::new(250.0, 600.0),
    );
    assert_on(
        tracker.step(&layout, VectorMm::new(-175.0, -10.0)),
        a.id,
        PointDevice::new(500.0, 500.0),
    );
    assert_on(
        tracker.step(&layout, VectorMm::new(1_000.0, 0.0)),
        c.id,
        PointDevice::new(999.0, 500.0),
    );
    // Same-node transitions may precede a crossing in the same long step.
    let d = panel(2, 1, 300.0, 0.0, 100.0, 100.0);
    let layout = self::layout(vec![a, b, c, d]);
    let mut tracker = PointerTracker::new(&layout, a.id, PointDevice::new(500.0, 500.0)).unwrap();
    assert!(
        matches!(tracker.step(&layout, VectorMm::new(1_000.0, 0.0)), Step::Crossed { display, .. } if display == d.id)
    );
}

#[test]
fn five_mm_gap_clamps_and_slides() {
    let a = panel(1, 1, 0.0, 0.0, 100.0, 100.0);
    let b = panel(2, 1, 105.0, 0.0, 100.0, 100.0);
    let layout = layout(vec![a, b]);
    assert!(layout.portals().is_empty());
    assert!(layout.locate(PointMm::new(102.0, 50.0)).is_none());
    let mut tracker = PointerTracker::new(&layout, a.id, PointDevice::new(500.0, 500.0)).unwrap();
    assert_on(
        tracker.step(&layout, VectorMm::new(10_000.0, 20.0)),
        a.id,
        PointDevice::new(999.0, 700.0),
    );
    assert_on(
        tracker.step(&layout, VectorMm::new(-10_000.0, 10_000.0)),
        a.id,
        PointDevice::new(0.0, 999.0),
    );
    // Touching includes a 2 mm gap or overlap; slightly more is not touching.
    for distance in [-TOUCH_TOLERANCE_MM, TOUCH_TOLERANCE_MM, 2.001] {
        let b = Placed {
            origin: PointMm::new(100.0 + distance, 0.0),
            ..b
        };
        let layout = self::layout(vec![a, b]);
        assert_eq!(
            layout.portals().len(),
            if distance.abs() <= TOUCH_TOLERANCE_MM {
                2
            } else {
                0
            }
        );
        let mut tracker =
            PointerTracker::new(&layout, a.id, PointDevice::new(500.0, 500.0)).unwrap();
        let step = tracker.step(&layout, VectorMm::new(1_000.0, 0.0));
        assert_eq!(
            matches!(step, Step::Crossed { .. }),
            distance.abs() <= TOUCH_TOLERANCE_MM
        );
    }
}

#[test]
fn overlap_is_rejected() {
    let a = panel(1, 1, 0.0, 0.0, 100.0, 100.0);
    let b = panel(2, 1, 97.0, 50.0, 100.0, 100.0);
    assert_eq!(
        Layout::new(vec![a, b], LayoutOptions::default()),
        Err(LayoutError::Overlap(a.id, b.id))
    );
    let same_node = Placed { id: id(1, 2), ..b };
    assert_eq!(
        Layout::new(vec![a, same_node], LayoutOptions::default()),
        Err(LayoutError::Overlap(a.id, same_node.id))
    );
}

#[test]
fn duplicate_id_is_rejected() {
    let a = panel(1, 1, 0.0, 0.0, 100.0, 100.0);
    let b = Placed {
        origin: PointMm::new(200.0, 0.0),
        ..a
    };
    assert_eq!(
        Layout::new(vec![a, b], LayoutOptions::default()),
        Err(LayoutError::Duplicate(a.id))
    );
}

#[test]
fn invalid_geometry_is_rejected() {
    let a = panel(1, 1, 0.0, 0.0, 100.0, 100.0);
    for value in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let geometry = DisplayGeometry {
            physical_size: SizeMm::new(value, 100.0),
            ..a.geometry
        };
        assert_eq!(
            Layout::new(vec![Placed { geometry, ..a }], LayoutOptions::default()),
            Err(LayoutError::InvalidGeometry(a.id))
        );
    }
    let geometry = DisplayGeometry {
        pixel_size: PixelSize::new(0, 1_000),
        ..a.geometry
    };
    assert_eq!(
        Layout::new(vec![Placed { geometry, ..a }], LayoutOptions::default()),
        Err(LayoutError::InvalidGeometry(a.id))
    );
    let invalid = Placed {
        origin: PointMm::new(f64::NAN, 0.0),
        ..a
    };
    assert_eq!(
        Layout::new(vec![invalid], LayoutOptions::default()),
        Err(LayoutError::InvalidGeometry(a.id))
    );
}

#[test]
fn dead_corners_trim_or_remove_portals() {
    let a = panel(1, 1, 0.0, 0.0, 100.0, 100.0);
    let b = panel(2, 1, 100.0, 40.0, 100.0, 20.0);
    let layout = Layout::new(
        vec![a, b],
        LayoutOptions {
            dead_corner_mm: 5.0,
        },
    )
    .unwrap();
    assert_eq!(layout.portals().len(), 2); // Exactly MIN_PORTAL_MM remains.
    near(layout.portals()[0].start, 450.0);
    near(layout.portals()[0].end, 550.0);
    near(layout.portals()[1].start, 250.0);
    near(layout.portals()[1].end, 750.0);
    let mut tracker = PointerTracker::new(&layout, a.id, PointDevice::new(500.0, 420.0)).unwrap();
    assert!(
        matches!(tracker.step(&layout, VectorMm::new(500.0, 0.0)), Step::On { display, .. } if display == a.id)
    );
    let mut tracker = PointerTracker::new(&layout, a.id, PointDevice::new(500.0, 500.0)).unwrap();
    assert!(
        matches!(tracker.step(&layout, VectorMm::new(500.0, 0.0)), Step::Crossed { display, .. } if display == b.id)
    );
    assert!(
        Layout::new(
            vec![a, b],
            LayoutOptions {
                dead_corner_mm: 5.001
            }
        )
        .unwrap()
        .portals()
        .is_empty()
    );
    let short = Placed {
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(100.0, 9.99),
            ..b.geometry
        },
        ..b
    };
    assert!(self::layout(vec![a, short]).portals().is_empty());
}

#[test]
fn portal_ids_are_deterministic() {
    let displays = vec![
        panel(1, 1, 0.0, 0.0, 100.0, 100.0),
        panel(2, 1, 100.0, 0.0, 100.0, 100.0),
        panel(3, 1, 0.0, 100.0, 100.0, 100.0),
        panel(4, 1, 100.0, 100.0, 100.0, 100.0),
    ];
    let original = layout(displays.clone());
    let mut reversed = displays.clone();
    reversed.reverse();
    let mut rotated = displays;
    rotated.rotate_left(1);
    assert_eq!(original.portals(), layout(reversed).portals());
    assert_eq!(original.portals(), layout(rotated).portals());
    assert_eq!(original.portals().len(), 8);
    let edges: Vec<_> = original
        .portals()
        .iter()
        .map(|portal| portal.edge)
        .collect();
    assert_eq!(
        edges,
        vec![
            Edge::Right,
            Edge::Bottom,
            Edge::Left,
            Edge::Bottom,
            Edge::Right,
            Edge::Top,
            Edge::Left,
            Edge::Top
        ]
    );
    for (portal, id) in original.portals().iter().zip(1_u32..) {
        assert_eq!(portal.id, PortalId(id));
    }
}

// Each orientation uses an unequal pixel density to exercise canvas-mm distances.
fn hysteresis_case(edge: Edge) -> (Layout, GlobalDisplayId, GlobalDisplayId, VectorMm) {
    let a = panel(1, 1, 0.0, 0.0, 100.0, 200.0);
    let (x, y, outward) = match edge {
        Edge::Left => (-100.0, 0.0, VectorMm::new(-1.0, 0.0)),
        Edge::Right => (100.0, 0.0, VectorMm::new(1.0, 0.0)),
        Edge::Top => (0.0, -200.0, VectorMm::new(0.0, -1.0)),
        Edge::Bottom => (0.0, 200.0, VectorMm::new(0.0, 1.0)),
    };
    let b = panel(2, 1, x, y, 100.0, 200.0);
    (layout(vec![a, b]), a.id, b.id, outward)
}

fn enter(
    layout: &Layout,
    a: GlobalDisplayId,
    b: GlobalDisplayId,
    outward: VectorMm,
) -> PointerTracker {
    let mut tracker = PointerTracker::new(layout, a, PointDevice::new(500.0, 500.0)).unwrap();
    assert!(
        matches!(tracker.step(layout, outward * 300.0), Step::Crossed { display, .. } if display == b)
    );
    tracker
}

#[test]
fn entry_reverse_jitter_clamps_without_crossing() {
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let (layout, a, b, outward) = hysteresis_case(edge);
        let mut tracker = enter(&layout, a, b, outward);
        let before = layout.to_canvas(b, tracker.position().1).unwrap();
        assert!(
            matches!(tracker.step(&layout, outward * -0.5), Step::On { display, .. } if display == b)
        );
        let placed = layout.get(b).unwrap();
        let expected = placed.geometry.clamp_device(
            placed
                .geometry
                .mm_to_device((before - placed.origin).to_point() + outward * -0.5),
        );
        assert_position(tracker.position(), (b, expected));
    }
}

#[test]
fn inward_motion_rearms_deliberate_return() {
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let (layout, a, b, outward) = hysteresis_case(edge);
        for inward in [2.0, crosspane_input::layout::REARM_MM] {
            let mut tracker = enter(&layout, a, b, outward);
            assert!(
                matches!(tracker.step(&layout, outward * inward), Step::On { display, .. } if display == b)
            );
            assert!(
                matches!(tracker.step(&layout, outward * -3.0), Step::Crossed { display, .. } if display == a)
            );
        }
    }
}

#[test]
fn tracker_created_at_entry_disarms_near_edge() {
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let (layout, a, b, outward) = hysteresis_case(edge);
        let portal = layout
            .portals()
            .iter()
            .find(|portal| portal.from == a)
            .unwrap();
        let (_, entry) = layout.entry(portal.id, 0.5).unwrap();
        let mut tracker = PointerTracker::new(&layout, b, entry).unwrap();
        assert!(
            matches!(tracker.step(&layout, outward * -0.5), Step::On { display, .. } if display == b)
        );
        assert!(matches!(
            tracker.step(&layout, outward * 2.0),
            Step::On { .. }
        ));
        assert!(
            matches!(tracker.step(&layout, outward * -3.0), Step::Crossed { display, .. } if display == a)
        );
    }
}

#[test]
fn disarmed_edge_sliding_preserves_tangent_motion() {
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let (layout, a, b, outward) = hysteresis_case(edge);
        let mut tracker = enter(&layout, a, b, outward);
        let before = layout.to_canvas(b, tracker.position().1).unwrap();
        let tangent = VectorMm::new(outward.y * 7.0, outward.x * 7.0);
        assert!(
            matches!(tracker.step(&layout, outward * -0.5 + tangent), Step::On { display, .. } if display == b)
        );
        let after = layout.to_canvas(b, tracker.position().1).unwrap();
        if vertical(edge) {
            near(after.y - before.y, tangent.y);
        } else {
            near(after.x - before.x, tangent.x);
        }
        // Sliding alone never re-arms the normal direction.
        assert!(
            matches!(tracker.step(&layout, outward * -3.0), Step::On { display, .. } if display == b)
        );
    }
}
