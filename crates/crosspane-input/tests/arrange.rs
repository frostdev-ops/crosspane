#![allow(clippy::unwrap_used)]

use std::cell::Cell;

use crosspane_input::Edge;
use crosspane_input::arrange::{
    Side, arrange_node, arrange_node_tree, arrange_node_used_fallback, default_layout, merge,
    place_beside, placed,
};
use crosspane_input::layout::{Layout, LayoutOptions, Placed, TOUCH_TOLERANCE_MM};
use crosspane_protocol::msg::Placement;
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelSize, PointLogical, PointMm, RectLogical, SizeLogical, SizeMm,
};
use crosspane_types::id::{DisplayId, GlobalDisplayId, NodeId};
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestRunner};

const EPSILON: f64 = 1e-8;
const PROPERTY_CASES: u32 = 256;

fn near(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= EPSILON,
        "{actual} differs from {expected}"
    );
}

fn point(actual: PointMm, expected: (f64, f64)) {
    near(actual.x, expected.0);
    near(actual.y, expected.1);
}

fn display(
    id: u32,
    pixels: (u32, u32),
    physical: (f64, f64),
    scale: f64,
    logical_origin: (f64, f64),
) -> DisplayInfo {
    DisplayInfo {
        id: DisplayId(id),
        name: format!("display-{id}"),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(physical.0, physical.1),
            pixel_size: PixelSize::new(pixels.0, pixels.1),
            scale,
            logical_origin: PointLogical::new(logical_origin.0, logical_origin.1),
        },
        refresh_millihz: 60_000,
        color_space: ColorSpace::Srgb,
        hdr: false,
    }
}

fn desktop() -> Vec<DisplayInfo> {
    vec![
        display(0, (1_080, 1_920), (340.0, 600.0), 1.0, (0.0, 600.0)),
        display(1, (1_920, 1_080), (600.0, 340.0), 1.0, (1_840.0, 0.0)),
        display(2, (3_440, 1_440), (800.0, 340.0), 1.0, (1_080.0, 1_080.0)),
    ]
}

fn mac() -> DisplayInfo {
    display(0, (3_024, 1_964), (302.0, 196.0), 2.0, (0.0, 0.0))
}

fn arranged(node: NodeId, displays: &[DisplayInfo]) -> Vec<Placed> {
    arrange_node(displays)
        .into_iter()
        .map(|(id, origin)| Placed {
            id: GlobalDisplayId { node, display: id },
            geometry: displays
                .iter()
                .find(|display| display.id == id)
                .unwrap()
                .geometry,
            origin,
        })
        .collect()
}

fn layout(displays: Vec<Placed>) -> Layout {
    Layout::new(displays, LayoutOptions::default()).unwrap()
}

fn assert_same_placements(a: &[Placement], b: &[Placement]) {
    assert_eq!(a.len(), b.len());
    for (a, b) in a.iter().zip(b) {
        assert_eq!(
            (a.node, a.display, a.version),
            (b.node, b.display, b.version)
        );
        // Determinism requires identical bits, rather than a numerical approximation.
        assert_eq!(a.origin.x.to_bits(), b.origin.x.to_bits());
        assert_eq!(a.origin.y.to_bits(), b.origin.y.to_bits());
    }
}

#[test]
fn dev_desktop() {
    let displays = desktop();
    let origins = arrange_node(&displays);
    assert_eq!(
        origins.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![DisplayId(0), DisplayId(1), DisplayId(2)]
    );
    // DP-2 anchors at (0, 0). DP-3 is below it:
    // x = (1080 - 1840) * (600 / 1920) = -237.5, y = 340.
    // HDMI is left of DP-3: x = -237.5 - 340 = -577.5;
    // y = 340 + (600 - 1080) * (340 / 1440) = 680/3.
    // Normalization adds 577.5 to x; the minimum y is already zero.
    point(origins[0].1, (0.0, 680.0 / 3.0));
    point(origins[1].1, (577.5, 0.0));
    point(origins[2].1, (340.0, 340.0));
    assert!(!arrange_node_used_fallback(&displays));
    assert_eq!(
        arrange_node_tree(&displays),
        vec![
            (DisplayId(0), Some(DisplayId(2))),
            (DisplayId(1), None),
            (DisplayId(2), Some(DisplayId(1))),
        ]
    );
    let placed = arranged(NodeId([1; 32]), &displays);
    near(placed[0].rect().max().x, placed[2].rect().min().x);
    near(placed[1].rect().max().y, placed[2].rect().min().y);
    assert!(layout(placed).portals().is_empty());
}

#[test]
fn mac_and_invalid_geometry() {
    let mut mac = mac();
    mac.geometry.logical_origin = PointLogical::new(-100.0, -200.0);
    let origins = arrange_node(&[mac.clone()]);
    point(origins[0].1, (0.0, 0.0));
    near(mac.geometry.logical_size().width, 1_512.0);
    near(mac.geometry.logical_size().height, 982.0);
    assert!(arrange_node(&[]).is_empty());
    assert!(arrange_node_tree(&[]).is_empty());
    assert!(!arrange_node_used_fallback(&[]));

    let invalid = [
        DisplayGeometry {
            physical_size: SizeMm::new(0.0, 196.0),
            ..mac.geometry
        },
        DisplayGeometry {
            physical_size: SizeMm::new(302.0, f64::NAN),
            ..mac.geometry
        },
        DisplayGeometry {
            pixel_size: PixelSize::new(0, 1_964),
            ..mac.geometry
        },
        DisplayGeometry {
            scale: -1.0,
            ..mac.geometry
        },
        DisplayGeometry {
            logical_origin: PointLogical::new(f64::INFINITY, 0.0),
            ..mac.geometry
        },
    ];
    for geometry in invalid {
        let bad = DisplayInfo {
            id: DisplayId(9),
            geometry,
            ..mac.clone()
        };
        assert!(arrange_node(std::slice::from_ref(&bad)).is_empty());
        assert!(!arrange_node_used_fallback(std::slice::from_ref(&bad)));
        let displays = [bad, mac.clone()];
        let origins = arrange_node(&displays);
        assert_eq!(origins.len(), 1);
        assert_eq!(origins[0].0, mac.id);
        point(origins[0].1, (0.0, 0.0));
        assert_eq!(arrange_node_tree(&displays), vec![(mac.id, None)]);
    }
    // Numerically equal origins use the id tie-break, including signed zero. These identical
    // logical rectangles are disconnected under the edge rule and get separate components.
    let tied = [
        DisplayInfo {
            id: DisplayId(8),
            geometry: DisplayGeometry {
                logical_origin: PointLogical::new(0.0, -0.0),
                ..mac.geometry
            },
            ..mac.clone()
        },
        DisplayInfo {
            id: DisplayId(4),
            geometry: DisplayGeometry {
                logical_origin: PointLogical::zero(),
                ..mac.geometry
            },
            ..mac.clone()
        },
    ];
    let origins = arrange_node(&tied);
    point(origins[0].1, (302.0, 0.0));
    point(origins[1].1, (0.0, 0.0));
    layout(arranged(NodeId([1; 32]), &[mac]));
}

#[test]
fn mixed_scale() {
    let displays = vec![
        display(1, (1_920, 1_080), (500.0, 300.0), 1.0, (0.0, 0.0)),
        display(2, (3_840, 2_160), (600.0, 340.0), 2.0, (1_920.0, 0.0)),
    ];
    let origins = arrange_node(&displays);
    point(origins[0].1, (0.0, 0.0));
    point(origins[1].1, (500.0, 0.0));
    assert!(!arrange_node_used_fallback(&displays));
    let placed = arranged(NodeId([1; 32]), &displays);
    near(placed[0].rect().max().x, placed[1].rect().min().x);
    layout(placed);

    // A square cycle has two possible parents for id 3. Id 1 is visited before id 2,
    // regardless of input order, so id 3 must keep id 1 as its BFS parent.
    let square = vec![
        display(3, (2_000, 2_000), (250.0, 250.0), 2.0, (1_000.0, 1_000.0)),
        display(2, (1_500, 1_500), (250.0, 250.0), 1.5, (1_000.0, 0.0)),
        display(1, (1_250, 1_250), (250.0, 250.0), 1.25, (0.0, 1_000.0)),
        display(0, (1_000, 1_000), (250.0, 250.0), 1.0, (0.0, 0.0)),
    ];
    assert_eq!(
        arrange_node_tree(&square),
        vec![
            (DisplayId(3), Some(DisplayId(1))),
            (DisplayId(2), Some(DisplayId(0))),
            (DisplayId(1), Some(DisplayId(0))),
            (DisplayId(0), None),
        ]
    );
    assert!(!arrange_node_used_fallback(&square));
    layout(arranged(NodeId([1; 32]), &square));
}

#[test]
fn disconnected_arrangement() {
    let displays = vec![
        display(0, (1_000, 1_000), (250.0, 250.0), 1.0, (0.0, 0.0)),
        display(1, (1_000, 1_000), (200.0, 250.0), 1.0, (4_000.0, 0.0)),
        display(2, (1_000, 1_000), (300.0, 250.0), 1.0, (3_000.0, 500.0)),
    ];
    let origins = arrange_node(&displays);
    point(origins[0].1, (0.0, 0.0));
    // Component 2 has a left neighbour of its anchor. Translate its entire bounding box
    // to x=250, y=0, rather than leaving the neighbour over component 1.
    point(origins[1].1, (550.0, 0.0));
    point(origins[2].1, (250.0, 125.0));
    assert_eq!(
        arrange_node_tree(&displays),
        vec![
            (DisplayId(0), None),
            (DisplayId(1), None),
            (DisplayId(2), Some(DisplayId(1))),
        ]
    );
    assert!(!arrange_node_used_fallback(&displays));
    layout(arranged(NodeId([1; 32]), &displays));
}

#[test]
fn uniform_scale_fallback_uses_both_axes() {
    let displays = vec![
        display(0, (1_000, 1_000), (250.0, 250.0), 1.0, (0.0, 0.0)),
        display(1, (1_000, 1_000), (250.0, 350.0), 1.0, (1_000.0, 0.0)),
        display(2, (1_000, 1_000), (300.0, 250.0), 1.0, (0.0, 1_000.0)),
    ];
    assert!(arrange_node_used_fallback(&displays));
    // Largest fx is 0.30, but largest fy is 0.35; use 0.35 for both coordinates.
    let origins = arrange_node(&displays);
    point(origins[0].1, (0.0, 0.0));
    point(origins[1].1, (350.0, 0.0));
    point(origins[2].1, (0.0, 350.0));
    layout(arranged(NodeId([1; 32]), &displays));
}

#[test]
fn original_overlap_counterexample() {
    let displays = vec![
        display(0, (1_000, 1_000), (250.0, 250.0), 1.0, (0.0, 0.0)),
        display(1, (1_000, 1_000), (250.0, 300.0), 1.0, (1_000.0, 0.0)),
        display(2, (1_000, 1_000), (300.0, 250.0), 1.0, (0.0, 1_000.0)),
    ];
    // BFS and the old median factor (0.25) overlap by 50x50 mm.
    // The amended maximum factor (0.30) separates these rectangles.
    assert!(arrange_node_used_fallback(&displays));
    let origins = arrange_node(&displays);
    point(origins[1].1, (300.0, 0.0));
    point(origins[2].1, (0.0, 300.0));
    assert_eq!(
        arrange_node_tree(&displays),
        vec![
            (DisplayId(0), None),
            (DisplayId(1), Some(DisplayId(0))),
            (DisplayId(2), Some(DisplayId(0))),
        ]
    );
    layout(arranged(NodeId([1; 32]), &displays));
}

#[test]
fn non_parent_adjacency_gap_is_accepted() {
    let displays = vec![
        display(0, (1_000, 2_000), (250.0, 500.0), 1.0, (0.0, 0.0)),
        display(1, (1_000, 1_000), (250.0, 200.0), 1.0, (1_000.0, 0.0)),
        display(2, (1_000, 1_000), (250.0, 250.0), 1.0, (1_000.0, 1_000.0)),
    ];
    assert!(!arrange_node_used_fallback(&displays));
    assert_eq!(
        arrange_node_tree(&displays),
        vec![
            (DisplayId(0), None),
            (DisplayId(1), Some(DisplayId(0))),
            (DisplayId(2), Some(DisplayId(0))),
        ]
    );
    // A places both children using its fy=500/2000=0.25. B and C touch logically,
    // but neither is the other's BFS parent: their accepted canvas gap is 50 mm.
    let placed = arranged(NodeId([1; 32]), &displays);
    point(placed[1].origin, (250.0, 0.0));
    point(placed[2].origin, (250.0, 250.0));
    near(placed[1].rect().min().x, placed[0].rect().max().x);
    near(placed[2].rect().min().x, placed[0].rect().max().x);
    near(placed[2].rect().min().y - placed[1].rect().max().y, 50.0);
    layout(placed);
}

#[test]
fn place_beside_on_all_four_sides() {
    let a_node = NodeId([1; 32]);
    let b_node = NodeId([2; 32]);
    let a = arranged(a_node, &desktop());
    let b = vec![(mac(), PointMm::zero())];
    for (side, edge, reverse, facing, expected) in [
        (Side::Right, Edge::Right, Edge::Left, 1, (1_177.5, 72.0)),
        (
            Side::Left,
            Edge::Left,
            Edge::Right,
            0,
            (-302.0, 1_286.0 / 3.0),
        ),
        (Side::Above, Edge::Top, Edge::Bottom, 1, (726.5, -196.0)),
        (
            Side::Below,
            Edge::Bottom,
            Edge::Top,
            0,
            (19.0, 2_480.0 / 3.0),
        ),
    ] {
        let origins = place_beside(&a, &b, side);
        point(origins[0], expected);
        let mac = Placed {
            id: GlobalDisplayId {
                node: b_node,
                display: b[0].0.id,
            },
            geometry: b[0].0.geometry,
            origin: origins[0],
        };
        let facing = &a[facing];
        if matches!(side, Side::Left | Side::Right) {
            near(facing.rect().center().y, mac.rect().center().y);
        } else {
            near(facing.rect().center().x, mac.rect().center().x);
        }
        let mut both = a.clone();
        both.push(mac);
        let layout = layout(both);
        assert!(layout.portals().len() >= 2);
        assert!(layout.portals().iter().any(|portal| {
            portal.from == facing.id && portal.to == mac.id && portal.edge == edge
        }));
        assert!(layout.portals().iter().any(|portal| {
            portal.from == mac.id && portal.to == facing.id && portal.edge == reverse
        }));
        point(place_beside(&[], &b, side)[0], (0.0, 0.0));
        assert!(place_beside(&a, &[], side).is_empty());
    }
}

#[test]
fn facing_display_ties_prefer_span_then_id() {
    let make = |id, y, height| {
        let display = display(id, (1_000, 1_000), (100.0, height), 1.0, (0.0, 0.0));
        Placed {
            id: GlobalDisplayId {
                node: NodeId([1; 32]),
                display: display.id,
            },
            geometry: display.geometry,
            origin: PointMm::new(0.0, y),
        }
    };
    let a = vec![
        make(3, 0.0, 50.0),
        make(2, 100.0, 100.0),
        make(1, 300.0, 100.0),
    ];
    let b = vec![
        (
            display(3, (1_000, 1_000), (20.0, 40.0), 1.0, (0.0, 0.0)),
            PointMm::new(10.0, 0.0),
        ),
        (
            display(2, (1_000, 1_000), (20.0, 80.0), 1.0, (0.0, 0.0)),
            PointMm::new(10.0, 100.0),
        ),
        (
            display(1, (1_000, 1_000), (20.0, 80.0), 1.0, (0.0, 0.0)),
            PointMm::new(10.0, 300.0),
        ),
    ];
    // Both facing displays have id 1. Their centres (350 and 340) give tangent shift +10.
    for (origin, expected) in place_beside(&a, &b, Side::Right).into_iter().zip([
        (100.0, 10.0),
        (100.0, 110.0),
        (100.0, 310.0),
    ]) {
        point(origin, expected);
    }
    let a: Vec<_> = a
        .into_iter()
        .map(|mut display| {
            display.origin = PointMm::new(display.origin.y, display.origin.x);
            let size = display.geometry.physical_size;
            display.geometry.physical_size = SizeMm::new(size.height, size.width);
            display
        })
        .collect();
    let b: Vec<_> = b
        .into_iter()
        .map(|(mut display, origin)| {
            let size = display.geometry.physical_size;
            display.geometry.physical_size = SizeMm::new(size.height, size.width);
            (display, PointMm::new(origin.y, origin.x))
        })
        .collect();
    for (origin, expected) in place_beside(&a, &b, Side::Below).into_iter().zip([
        (10.0, 100.0),
        (110.0, 100.0),
        (310.0, 100.0),
    ]) {
        point(origin, expected);
    }
}

#[test]
fn deterministic_default_layout() {
    let small = NodeId([1; 32]);
    let large = NodeId([2; 32]);
    let nodes = vec![(large, vec![mac()]), (small, desktop())];
    let result = default_layout(&nodes);
    let mut reversed = nodes.clone();
    reversed.reverse();
    reversed[0].1.reverse();
    assert_same_placements(&result, &default_layout(&reversed));
    reversed.push((NodeId([0; 32]), vec![]));
    assert_same_placements(&result, &default_layout(&reversed));
    assert!(default_layout(&[]).is_empty());
    assert!(result.iter().all(|entry| entry.version == 0));
    assert_eq!(result[0].node, small);
    let displays = placed(&result, &nodes);
    let small_right = displays
        .iter()
        .filter(|entry| entry.id.node == small)
        .map(|entry| entry.rect().max().x)
        .fold(f64::NEG_INFINITY, f64::max);
    let large_left = displays
        .iter()
        .find(|entry| entry.id.node == large)
        .unwrap()
        .origin
        .x;
    near(small_right, large_left);
    let layout = layout(displays);
    assert!(layout.portals().iter().any(|portal| {
        portal.from.node == small && portal.to.node == large && portal.edge == Edge::Right
    }));
    assert!(layout.portals().iter().any(|portal| {
        portal.from.node == large && portal.to.node == small && portal.edge == Edge::Left
    }));
}

fn placement(node: u8, display: u32, origin: (f64, f64), version: u64) -> Placement {
    Placement {
        node: NodeId([node; 32]),
        display: DisplayId(display),
        origin: PointMm::new(origin.0, origin.1),
        version,
    }
}

#[test]
fn merge_versions_ties_and_sorting() {
    let old = placement(2, 1, (10.0, 20.0), 1);
    let newer = placement(2, 1, (30.0, 40.0), 2);
    let mut current = vec![old];
    assert!(merge(&mut current, &[newer]));
    assert_same_placements(&current, &[newer]);
    assert!(!merge(&mut current, &[old, newer]));

    let a = placement(1, 4, (-1.0, 10.0), 7);
    let b = placement(1, 4, (1.0, 20.0), 7);
    let mut left = vec![a];
    let mut right = vec![b];
    assert!(merge(&mut left, &[b]));
    assert!(!merge(&mut right, &[a]));
    assert_same_placements(&left, &right);
    assert_same_placements(&left, &[b]); // Unsigned bits, not numerical coordinate order.
    let y_winner = placement(1, 4, (1.0, 1.0), 7);
    assert!(merge(&mut left, &[y_winner]));
    assert_same_placements(&left, &[y_winner]);

    let negative_zero = placement(1, 0, (-0.0, 0.0), 0);
    let positive_zero = placement(1, 0, (0.0, 0.0), 0);
    let mut zeros = vec![negative_zero];
    assert!(merge(&mut zeros, &[positive_zero]));
    assert!(!merge(&mut zeros, &[negative_zero, positive_zero]));
    assert_same_placements(&zeros, &[positive_zero]);

    current.extend([
        placement(3, 0, (0.0, 0.0), 0),
        placement(1, 5, (0.0, 0.0), 0),
    ]);
    assert!(merge(&mut current, &[placement(1, 0, (0.0, 0.0), 0)]));
    assert!(
        current
            .windows(2)
            .all(|pair| { (pair[0].node, pair[0].display) < (pair[1].node, pair[1].display) })
    );
    assert!(!merge(&mut current, &[]));
}

#[test]
fn placed_skips_unknown_displays_and_unplaced_reports() {
    let nodes = vec![(NodeId([1; 32]), desktop()), (NodeId([2; 32]), vec![mac()])];
    let placements = vec![
        placement(1, 99, (0.0, 0.0), 0),
        placement(2, 0, (20.0, 30.0), 1),
        placement(3, 0, (0.0, 0.0), 0),
        placement(1, 0, (40.0, 50.0), 2),
    ];
    let result = placed(&placements, &nodes);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].id.node, NodeId([2; 32]));
    assert_eq!(result[1].id.node, NodeId([1; 32]));
    assert!(result.iter().all(|entry| entry.id.display == DisplayId(0)));
    assert_eq!(result[0].geometry.pixel_size, PixelSize::new(3_024, 1_964));
    assert_eq!(result[1].geometry.pixel_size, PixelSize::new(1_080, 1_920));
    point(result[0].origin, (20.0, 30.0));
    point(result[1].origin, (40.0, 50.0));
    assert!(placed(&[], &nodes).is_empty());
    assert!(placed(&placements, &[]).is_empty());
}

fn attach(parent: RectLogical, size: SizeLogical, edge: u8, fraction: f64) -> PointLogical {
    // Strictly inside the range of offsets with positive shared-axis overlap.
    let x = parent.min().x - size.width + fraction * (parent.size.width + size.width);
    let y = parent.min().y - size.height + fraction * (parent.size.height + size.height);
    match edge {
        0 => PointLogical::new(parent.max().x, y),
        1 => PointLogical::new(parent.min().x - size.width, y),
        2 => PointLogical::new(x, parent.max().y),
        _ => PointLogical::new(x, parent.min().y - size.height),
    }
}

fn logical_overlap(a: RectLogical, b: RectLogical) -> bool {
    a.max().x.min(b.max().x) - a.min().x.max(b.min().x) > EPSILON
        && a.max().y.min(b.max().y) - a.min().y.max(b.min().y) > EPSILON
}

fn random_displays() -> impl Strategy<Value = Vec<DisplayInfo>> {
    let step = (
        100_u32..1_001,
        100_u32..1_001,
        0_usize..4,
        70_u32..=130,
        70_u32..=130,
        0_usize..4,
        0_u8..4,
        1_u16..1_000,
    );
    (
        -2_000_i32..2_001,
        -2_000_i32..2_001,
        prop::collection::vec(step, 1..=4),
    )
        .prop_map(|(x, y, steps)| {
            let mut displays: Vec<DisplayInfo> = Vec::new();
            for (index, (width, height, scale, fx, fy, parent, edge, offset)) in
                steps.into_iter().enumerate()
            {
                // Multiples of four give integral pixel sizes at every requested scale.
                let logical = SizeLogical::new(f64::from(width * 4), f64::from(height * 4));
                let quarter_scale = [4, 5, 6, 8][scale];
                let pixels = (width * quarter_scale, height * quarter_scale);
                let origin = if displays.is_empty() {
                    PointLogical::new(f64::from(x), f64::from(y))
                } else {
                    let parent = &displays[parent % displays.len()];
                    let candidate = attach(
                        parent.geometry.logical_bounds(),
                        logical,
                        edge,
                        f64::from(offset) / 1_000.0,
                    );
                    let rect = RectLogical::new(candidate, logical);
                    if displays
                        .iter()
                        .any(|display| logical_overlap(display.geometry.logical_bounds(), rect))
                    {
                        // A collision chooses a display on this outermost edge instead. Attaching
                        // outside it is always safe and connected: no filtering or prop_assume.
                        let parent = displays
                            .iter()
                            .max_by(|a, b| {
                                let a = a.geometry.logical_bounds();
                                let b = b.geometry.logical_bounds();
                                match edge {
                                    0 => a.max().x.total_cmp(&b.max().x),
                                    1 => b.min().x.total_cmp(&a.min().x),
                                    2 => a.max().y.total_cmp(&b.max().y),
                                    _ => b.min().y.total_cmp(&a.min().y),
                                }
                            })
                            .unwrap();
                        attach(parent.geometry.logical_bounds(), logical, edge, 0.5)
                    } else {
                        candidate
                    }
                };
                displays.push(display(
                    index as u32,
                    pixels,
                    (
                        f64::from(pixels.0) * 25.4 / 96.0 * f64::from(fx) / 100.0,
                        f64::from(pixels.1) * 25.4 / 96.0 * f64::from(fy) / 100.0,
                    ),
                    f64::from(quarter_scale) / 4.0,
                    (origin.x, origin.y),
                ));
            }
            displays
        })
}

fn parent_edge_distance(parent: &Placed, child: &Placed) -> Option<f64> {
    let a = parent.geometry.logical_bounds();
    let b = child.geometry.logical_bounds();
    let overlap_y = a.max().y.min(b.max().y) - a.min().y.max(b.min().y);
    let overlap_x = a.max().x.min(b.max().x) - a.min().x.max(b.min().x);
    let a_mm = parent.rect();
    let b_mm = child.rect();
    if overlap_y > 0.0 && (a.max().x - b.min().x).abs() <= 0.5 {
        Some((a_mm.max().x - b_mm.min().x).abs())
    } else if overlap_y > 0.0 && (a.min().x - b.max().x).abs() <= 0.5 {
        Some((a_mm.min().x - b_mm.max().x).abs())
    } else if overlap_x > 0.0 && (a.max().y - b.min().y).abs() <= 0.5 {
        Some((a_mm.max().y - b_mm.min().y).abs())
    } else if overlap_x > 0.0 && (a.min().y - b.max().y).abs() <= 0.5 {
        Some((a_mm.min().y - b_mm.max().y).abs())
    } else {
        None
    }
}

#[test]
fn random_connected_rectangles() {
    // Counters surround the runner so stderr reports the complete successful run, without
    // prop_assume, rejected cases, global state, or dependence on other tests' execution order.
    let cases = Cell::new(0_usize);
    let fallbacks = Cell::new(0_usize);
    let mut runner = TestRunner::new(ProptestConfig {
        cases: PROPERTY_CASES,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(139),
        ..ProptestConfig::default()
    });
    let result = runner.run(&random_displays(), |displays| {
        cases.set(cases.get() + 1);
        for (index, display) in displays.iter().enumerate() {
            for other in &displays[..index] {
                prop_assert!(!logical_overlap(
                    display.geometry.logical_bounds(),
                    other.geometry.logical_bounds()
                ));
            }
        }
        let tree = arrange_node_tree(&displays);
        prop_assert_eq!(tree.len(), displays.len());
        prop_assert_eq!(
            tree.iter().filter(|(_, parent)| parent.is_none()).count(),
            1
        );
        let used_fallback = arrange_node_used_fallback(&displays);
        if used_fallback {
            fallbacks.set(fallbacks.get() + 1);
        }
        let arranged = arranged(NodeId([1; 32]), &displays);
        prop_assert!(Layout::new(arranged.clone(), LayoutOptions::default()).is_ok());
        if !used_fallback {
            for (child, parent) in tree {
                if let Some(parent) = parent {
                    let child = arranged
                        .iter()
                        .find(|entry| entry.id.display == child)
                        .unwrap();
                    let parent = arranged
                        .iter()
                        .find(|entry| entry.id.display == parent)
                        .unwrap();
                    let distance = parent_edge_distance(parent, child);
                    prop_assert!(
                        distance.is_some_and(|distance| distance <= TOUCH_TOLERANCE_MM + EPSILON)
                    );
                }
            }
        }
        Ok(())
    });
    eprintln!(
        "arrange fallback rate: {}/{} ({:.2}%)",
        fallbacks.get(),
        cases.get(),
        100.0 * fallbacks.get() as f64 / cases.get() as f64,
    );
    result.unwrap();
    assert_eq!(cases.get(), PROPERTY_CASES as usize);
}
