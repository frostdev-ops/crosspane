//! Visual contact spans only. Input gating, dead corners and portal policy stay in the agent.
use crate::layout::DisplayRect;

/// A side of a display rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// The left side.
    Left,
    /// The right side.
    Right,
    /// The top side.
    Top,
    /// The bottom side.
    Bottom,
}

/// A contact span between two displays of different machines, in millimetres.
#[derive(Clone, Debug, PartialEq)]
pub struct Crossing {
    /// Indices into the display slice given to [`crossings`], the lower index first.
    pub displays: [usize; 2],
    /// The touching edge of each display, in the same order as `displays`.
    pub edges: [Edge; 2],
    /// One end of the shared span, on the midpoint line between the two edges.
    pub start: [f64; 2],
    /// The other end of the shared span.
    pub end: [f64; 2],
}

/// The input layout regards opposing edges within 2 mm as touching. Paint their shared
/// span at the midpoint; do not imply that a gate is open or that a portal is armed.
///
/// Only visible displays of different machines with a positive shared span count: corner
/// contacts, same-machine neighbours and displays without a known size never cross.
pub fn crossings(displays: &[DisplayRect]) -> Vec<Crossing> {
    let mut result = Vec::new();
    for (i, a) in displays.iter().enumerate() {
        if !a.visible() {
            continue;
        }
        for (j, b) in displays.iter().enumerate().skip(i + 1) {
            if !b.visible() || a.node == b.node {
                continue;
            }
            for axis in 0..2 {
                let other = 1 - axis;
                let start = a.origin[other].max(b.origin[other]);
                let end = (a.origin[other] + a.size[other]).min(b.origin[other] + b.size[other]);
                if start >= end {
                    continue;
                }
                for (a_end, b_end) in [(true, false), (false, true)] {
                    let ae = a.origin[axis] + if a_end { a.size[axis] } else { 0.0 };
                    let be = b.origin[axis] + if b_end { b.size[axis] } else { 0.0 };
                    if (ae - be).abs() > 2.0 {
                        continue;
                    }
                    let edges = match (axis, a_end) {
                        (0, true) => [Edge::Right, Edge::Left],
                        (0, false) => [Edge::Left, Edge::Right],
                        (_, true) => [Edge::Bottom, Edge::Top],
                        (_, false) => [Edge::Top, Edge::Bottom],
                    };
                    let mut from = [0.0; 2];
                    let mut to = [0.0; 2];
                    from[axis] = (ae + be) / 2.0;
                    to[axis] = from[axis];
                    from[other] = start;
                    to[other] = end;
                    result.push(Crossing {
                        displays: [i, j],
                        edges,
                        start: from,
                        end: to,
                    });
                }
            }
        }
    }
    result
}
