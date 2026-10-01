//! Deterministic display arrangements on the shared millimetre canvas.
//!
//! BFS preserves the normal position of each parent/child edge. Other logical adjacencies can
//! have gaps. If BFS overlaps, a uniform maximum-factor fallback trades adjacency for separation
//! (provided the input logical rectangles do not overlap). Physical panel sizes never change.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crosspane_protocol::msg::Placement;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{PointMm, RectLogical, SizeMm};
use crosspane_types::id::{DisplayId, GlobalDisplayId, NodeId};

use crate::layout::{Placed, TOUCH_TOLERANCE_MM};

const LOGICAL_TOUCH_TOLERANCE: f64 = 0.5;

/// Where node B sits relative to node A.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Above,
    Below,
}

impl Side {
    fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Above => Self::Below,
            Self::Below => Self::Above,
        }
    }

    fn vertical_edge(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }

    fn maximum_edge(self) -> bool {
        matches!(self, Self::Right | Self::Below)
    }
}

struct ArrangedDisplay<'a> {
    display: &'a DisplayInfo,
    logical: RectLogical,
    fx: f64,
    fy: f64,
    origin: PointMm,
    parent: Option<DisplayId>,
}

struct Arrangement<'a> {
    displays: Vec<ArrangedDisplay<'a>>,
    used_fallback: bool,
}

fn coordinate_order(a: f64, b: f64) -> Ordering {
    // Signed zeros are the same coordinate and must reach the id tie-break.
    a.partial_cmp(&b).unwrap_or_else(|| a.total_cmp(&b))
}

/// Place valid displays at their physical sizes, in input order, with the bounding box at (0, 0).
/// Empty input gives empty output. Invalid geometry is skipped.
///
/// BFS parent edges align. On overlap, the maximum-factor fallback can introduce gaps.
pub fn arrange_node(displays: &[DisplayInfo]) -> Vec<(DisplayId, PointMm)> {
    arrange(displays)
        .displays
        .into_iter()
        .map(|entry| (entry.display.id, entry.origin))
        .collect()
}

/// Whether BFS produced an overlap and the uniform maximum-factor fallback was used.
pub fn arrange_node_used_fallback(displays: &[DisplayInfo]) -> bool {
    arrange(displays).used_fallback
}

/// Each valid display and its BFS parent, in input order; component roots have no parent.
/// The tree describes BFS even when the final placement uses the overlap fallback.
pub fn arrange_node_tree(displays: &[DisplayInfo]) -> Vec<(DisplayId, Option<DisplayId>)> {
    arrange(displays)
        .displays
        .into_iter()
        .map(|entry| (entry.display.id, entry.parent))
        .collect()
}

fn arrange(displays: &[DisplayInfo]) -> Arrangement<'_> {
    let mut entries: Vec<_> = displays
        .iter()
        .filter(|display| display.geometry.is_valid())
        .map(|display| {
            let logical = display.geometry.logical_bounds();
            let physical = display.geometry.physical_size;
            ArrangedDisplay {
                display,
                logical,
                fx: physical.width / logical.size.width,
                fy: physical.height / logical.size.height,
                origin: PointMm::zero(),
                parent: None,
            }
        })
        .collect();

    let mut roots: Vec<_> = (0..entries.len()).collect();
    roots.sort_by(|&a, &b| {
        coordinate_order(entries[a].logical.origin.y, entries[b].logical.origin.y)
            .then_with(|| {
                coordinate_order(entries[a].logical.origin.x, entries[b].logical.origin.x)
            })
            .then_with(|| entries[a].display.id.cmp(&entries[b].display.id))
    });
    let mut neighbours = roots.clone();
    neighbours.sort_by_key(|&index| entries[index].display.id);
    let mut visited = vec![false; entries.len()];

    for root in roots {
        if visited[root] {
            continue;
        }
        let first_component = !visited.iter().any(|&value| value);
        let right = entries
            .iter()
            .zip(&visited)
            .filter(|(_, placed)| **placed)
            .map(|(entry, _)| entry.origin.x + entry.display.geometry.physical_size.width)
            .fold(0.0, f64::max);

        // A vector with a cursor is a FIFO queue and also retains this component's indices.
        let mut component = vec![root];
        visited[root] = true;
        let mut cursor = 0;
        while cursor < component.len() {
            let parent = component[cursor];
            cursor += 1;
            for &child in &neighbours {
                if visited[child] {
                    continue;
                }
                if let Some(side) = adjacent(entries[parent].logical, entries[child].logical) {
                    entries[child].origin =
                        neighbour_origin(&entries[parent], &entries[child], side);
                    entries[child].parent = Some(entries[parent].display.id);
                    visited[child] = true;
                    component.push(child);
                }
            }
        }
        if !first_component {
            let min_x = component
                .iter()
                .map(|&index| entries[index].origin.x)
                .fold(f64::INFINITY, f64::min);
            let min_y = component
                .iter()
                .map(|&index| entries[index].origin.y)
                .fold(f64::INFINITY, f64::min);
            for index in component {
                entries[index].origin.x += right - min_x;
                entries[index].origin.y -= min_y;
            }
        }
    }

    let used_fallback = entries
        .iter()
        .enumerate()
        .any(|(index, entry)| entries[..index].iter().any(|other| overlaps(entry, other)));
    if used_fallback {
        let factor = entries
            .iter()
            .map(|entry| entry.fx.max(entry.fy))
            .fold(0.0, f64::max);
        for entry in &mut entries {
            entry.origin = PointMm::new(
                entry.logical.origin.x * factor,
                entry.logical.origin.y * factor,
            );
        }
    }
    if !entries.is_empty() {
        let min_x = entries
            .iter()
            .map(|entry| entry.origin.x)
            .fold(f64::INFINITY, f64::min);
        let min_y = entries
            .iter()
            .map(|entry| entry.origin.y)
            .fold(f64::INFINITY, f64::min);
        for entry in &mut entries {
            entry.origin.x -= min_x;
            entry.origin.y -= min_y;
        }
    }
    Arrangement {
        displays: entries,
        used_fallback,
    }
}

fn adjacent(parent: RectLogical, child: RectLogical) -> Option<Side> {
    let overlap_y = parent.max().y.min(child.max().y) - parent.min().y.max(child.min().y);
    let overlap_x = parent.max().x.min(child.max().x) - parent.min().x.max(child.min().x);
    if overlap_y > 0.0 {
        if (parent.max().x - child.min().x).abs() <= LOGICAL_TOUCH_TOLERANCE {
            return Some(Side::Right);
        }
        if (parent.min().x - child.max().x).abs() <= LOGICAL_TOUCH_TOLERANCE {
            return Some(Side::Left);
        }
    }
    if overlap_x > 0.0 {
        if (parent.max().y - child.min().y).abs() <= LOGICAL_TOUCH_TOLERANCE {
            return Some(Side::Below);
        }
        if (parent.min().y - child.max().y).abs() <= LOGICAL_TOUCH_TOLERANCE {
            return Some(Side::Above);
        }
    }
    None
}

fn neighbour_origin(
    parent: &ArrangedDisplay<'_>,
    child: &ArrangedDisplay<'_>,
    side: Side,
) -> PointMm {
    let parent_size = parent.display.geometry.physical_size;
    let child_size = child.display.geometry.physical_size;
    let x = parent.origin.x + (child.logical.origin.x - parent.logical.origin.x) * parent.fx;
    let y = parent.origin.y + (child.logical.origin.y - parent.logical.origin.y) * parent.fy;
    match side {
        Side::Right => PointMm::new(parent.origin.x + parent_size.width, y),
        Side::Left => PointMm::new(parent.origin.x - child_size.width, y),
        Side::Below => PointMm::new(x, parent.origin.y + parent_size.height),
        Side::Above => PointMm::new(x, parent.origin.y - child_size.height),
    }
}

fn overlaps(a: &ArrangedDisplay<'_>, b: &ArrangedDisplay<'_>) -> bool {
    let a_size = a.display.geometry.physical_size;
    let b_size = b.display.geometry.physical_size;
    (a.origin.x + a_size.width).min(b.origin.x + b_size.width) - a.origin.x.max(b.origin.x)
        > TOUCH_TOLERANCE_MM
        && (a.origin.y + a_size.height).min(b.origin.y + b_size.height) - a.origin.y.max(b.origin.y)
            > TOUCH_TOLERANCE_MM
}

struct Face<K> {
    coordinate: f64,
    span: f64,
    centre: f64,
    id: K,
}

fn face<K>(origin: PointMm, size: SizeMm, side: Side, id: K) -> Face<K> {
    let coordinate = match side {
        Side::Left => origin.x,
        Side::Right => origin.x + size.width,
        Side::Above => origin.y,
        Side::Below => origin.y + size.height,
    };
    let (span, start) = if side.vertical_edge() {
        (size.height, origin.y)
    } else {
        (size.width, origin.x)
    };
    Face {
        coordinate,
        span,
        centre: start + span / 2.0,
        id,
    }
}

fn compare_faces<K: Ord>(a: &Face<K>, b: &Face<K>, side: Side) -> Ordering {
    let edge_order = coordinate_order(a.coordinate, b.coordinate);
    let edge_order = if side.maximum_edge() {
        edge_order.reverse()
    } else {
        edge_order
    };
    edge_order
        .then_with(|| b.span.total_cmp(&a.span))
        .then_with(|| a.id.cmp(&b.id))
}

/// Translate B to touch A on `side`, aligning the facing displays' centres.
/// Facing-edge ties prefer a longer shared-axis size, then a smaller display id.
/// If either input is empty, B's origins are returned unchanged, in input order.
pub fn place_beside(a: &[Placed], b: &[(DisplayInfo, PointMm)], side: Side) -> Vec<PointMm> {
    let a_face = a
        .iter()
        .map(|display| {
            face(
                display.origin,
                display.geometry.physical_size,
                side,
                display.id,
            )
        })
        .min_by(|left, right| compare_faces(left, right, side));
    let b_side = side.opposite();
    let b_face = b
        .iter()
        .map(|(display, origin)| face(*origin, display.geometry.physical_size, b_side, display.id))
        .min_by(|left, right| compare_faces(left, right, b_side));
    let (Some(a_face), Some(b_face)) = (a_face, b_face) else {
        return b.iter().map(|(_, origin)| *origin).collect();
    };
    let normal = a_face.coordinate - b_face.coordinate;
    let tangent = a_face.centre - b_face.centre;
    b.iter()
        .map(|(_, origin)| {
            if side.vertical_edge() {
                PointMm::new(origin.x + normal, origin.y + tangent)
            } else {
                PointMm::new(origin.x + tangent, origin.y + normal)
            }
        })
        .collect()
}

/// Deterministic default layout: nodes in bytewise id order, each to the right of its predecessors.
/// Placements are sorted by `(node, display)` and have version zero.
pub fn default_layout(nodes: &[(NodeId, Vec<DisplayInfo>)]) -> Vec<Placement> {
    let mut nodes: Vec<_> = nodes.iter().collect();
    nodes.sort_by_key(|(node, _)| *node);
    let mut previous = Vec::new();
    let mut placements = Vec::new();
    for (node, displays) in nodes {
        let arranged: Vec<_> = arrange_node(displays)
            .into_iter()
            .filter_map(|(id, origin)| {
                displays
                    .iter()
                    .find(|display| display.id == id)
                    .map(|display| (display.clone(), origin))
            })
            .collect();
        let origins = place_beside(&previous, &arranged, Side::Right);
        for ((display, _), origin) in arranged.into_iter().zip(origins) {
            placements.push(Placement {
                node: *node,
                display: display.id,
                origin,
                version: 0,
            });
            previous.push(Placed {
                id: GlobalDisplayId {
                    node: *node,
                    display: display.id,
                },
                geometry: display.geometry,
                origin,
            });
        }
    }
    placements.sort_by_key(|entry| (entry.node, entry.display));
    placements
}

fn origin_bits(placement: &Placement) -> (u64, u64) {
    (placement.origin.x.to_bits(), placement.origin.y.to_bits())
}

/// Merge by `(node, display)`, preferring higher versions, then smaller origin bit tuples.
/// The result stays sorted; the return value includes changes to ordering or origin bits.
pub fn merge(current: &mut Vec<Placement>, incoming: &[Placement]) -> bool {
    let mut entries = BTreeMap::<_, Placement>::new();
    for update in current.iter().chain(incoming) {
        entries
            .entry((update.node, update.display))
            .and_modify(|existing| {
                if update.version > existing.version
                    || (update.version == existing.version
                        && origin_bits(update) < origin_bits(existing))
                {
                    *existing = *update;
                }
            })
            .or_insert(*update);
    }
    let merged: Vec<_> = entries.into_values().collect();
    let changed = current.len() != merged.len()
        || current.iter().zip(&merged).any(|(old, new)| {
            old.node != new.node
                || old.display != new.display
                || old.version != new.version
                || origin_bits(old) != origin_bits(new)
        });
    *current = merged;
    changed
}

/// Match placements to the displays reported by their nodes, skipping unmatched entries.
/// Displays without a placement are omitted. Placement input order is preserved.
pub fn placed(placements: &[Placement], displays: &[(NodeId, Vec<DisplayInfo>)]) -> Vec<Placed> {
    placements
        .iter()
        .filter_map(|placement| {
            let (_, node_displays) = displays.iter().find(|(node, _)| *node == placement.node)?;
            let display = node_displays
                .iter()
                .find(|display| display.id == placement.display)?;
            Some(Placed {
                id: GlobalDisplayId {
                    node: placement.node,
                    display: placement.display,
                },
                geometry: display.geometry,
                origin: placement.origin,
            })
        })
        .collect()
}
