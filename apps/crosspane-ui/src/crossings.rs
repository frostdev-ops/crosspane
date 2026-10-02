//! Visual contact spans only. Input gating, dead corners and portal policy stay in the agent.
use crate::layout::DisplayRect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Crossing {
    pub displays: [usize; 2],
    pub edges: [Edge; 2],
    pub start: [f64; 2],
    pub end: [f64; 2],
}

/// The input layout regards opposing edges within 2 mm as touching. Paint their shared
/// span at the midpoint; do not imply that a gate is open or that a portal is armed.
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

#[cfg(test)]
mod tests {
    use super::*;
    fn display(node: &str, origin: [f64; 2], size: [f64; 2]) -> DisplayRect {
        DisplayRect {
            node: node.into(),
            machine: node.into(),
            display: 0,
            name: String::new(),
            origin,
            size,
        }
    }
    #[test]
    fn all_opposing_edges_and_partial_spans() {
        let a = display("a", [0.0, 0.0], [100.0, 100.0]);
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
                crossings(&[a.clone(), display("b", origin, [100.0; 2])]),
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
        let a = display("a", [0.0; 2], [100.0; 2]);
        for b in [
            display("b", [100.0; 2], [100.0; 2]),
            display("a", [100.0, 0.0], [100.0; 2]),
            display("b", [0.0; 2], [0.0; 2]),
            display("b", [102.01, 0.0], [100.0; 2]),
            display("b", [50.0; 2], [100.0; 2]),
        ] {
            assert!(crossings(&[a.clone(), b]).is_empty());
        }
        let touching = crossings(&[a, display("b", [102.0, 0.0], [100.0; 2])]);
        assert_eq!(touching[0].start, [101.0, 0.0]);
    }
    #[test]
    fn stepped_multi_display_contacts_keep_display_indices() {
        let spans = crossings(&[
            display("a", [0.0; 2], [100.0; 2]),
            display("a", [0.0, 100.0], [100.0; 2]),
            display("b", [100.0, 50.0], [100.0; 2]),
        ]);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].displays, [0, 2]);
        assert_eq!(spans[1].displays, [1, 2]);
        assert_eq!(spans[0].end, [100.0, 100.0]);
        assert_eq!(spans[1].start, [100.0, 100.0]);
    }
}
