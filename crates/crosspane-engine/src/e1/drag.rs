//! Private drag ownership and placement; no held-at-seat button enters the router.

use crate::io::ProjectionKey;
use crosspane_input::Edge;
use crosspane_protocol::projection::ProxyPlacement;
use crosspane_types::geom::{DisplayGeometry, PixelSize, PointDevice, VectorMm};
use crosspane_types::id::{GlobalDisplayId, NodeId, WindowId};
use crosspane_types::time::MonoTime;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Kind {
    Out(WindowId),
    Back(ProjectionKey),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Offer {
    pub window: WindowId,
    pub kind: Kind,
    pub peer: NodeId,
    pub size: PixelSize,
    pub scale: f64,
}

impl Offer {
    /// Retiling changes placement geometry, not which gesture is being accepted.
    pub(crate) fn same_gesture(self, other: Self) -> bool {
        self.window == other.window && self.kind == other.kind && self.peer == other.peer
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Stage {
    Pending,
    Continuing {
        key: ProjectionKey,
        token: u32,
        until: MonoTime,
    },
    Pressed {
        key: ProjectionKey,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Drag {
    pub portal: crosspane_platform::PortalId,
    pub offer: Offer,
    pub grab: PointDevice,
    pub edge: Edge,
    pub entry: GlobalDisplayId,
    pub stage: Stage,
    pub motion: VectorMm,
}

impl Drag {
    pub(crate) fn refresh_offer(&mut self, offer: Offer) {
        // Preserve the original logical grab when its source's device scale changes.
        let ratio = offer.scale / self.offer.scale;
        self.grab = PointDevice::new(self.grab.x * ratio, self.grab.y * ratio);
        self.offer = offer;
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Commit {
    pub kind: Kind,
    pub peer: NodeId,
    pub place: ProxyPlacement,
    pub token: u32,
    pub anchor: (i32, i32),
    pub size: PixelSize,
}

pub(crate) fn inward(edge: Edge, p: PointDevice, geometry: DisplayGeometry) -> f64 {
    let distance = match edge {
        Edge::Right => p.x,
        Edge::Left => f64::from(geometry.pixel_size.width) - p.x,
        Edge::Bottom => p.y,
        Edge::Top => f64::from(geometry.pixel_size.height) - p.y,
    };
    distance / geometry.scale
}

pub(crate) fn clamped_motion(
    layout: &crosspane_input::layout::Layout,
    previous: (GlobalDisplayId, PointDevice),
    mm: VectorMm,
) -> PointDevice {
    layout.get(previous.0).map_or(previous.1, |p| {
        p.geometry.clamp_device(
            p.geometry
                .mm_to_device(p.geometry.device_to_mm(previous.1) + mm),
        )
    })
}

pub(crate) fn placement(
    drag: Drag,
    p: PointDevice,
    geometry: DisplayGeometry,
    token: u32,
) -> Commit {
    let ratio = geometry.scale / drag.offer.scale;
    let size = PixelSize::new(
        (f64::from(drag.offer.size.width) * ratio)
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32,
        (f64::from(drag.offer.size.height) * ratio)
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32,
    );
    let out = matches!(drag.offer.kind, Kind::Out(_));
    let anchor = |value: f64, length: f64| {
        let inset = 8.0_f64.min(((length - 1.0).max(0.0) / 2.0).floor());
        value.clamp(inset, (length - 1.0 - inset).max(inset))
    };
    let grab = PointDevice::new(drag.grab.x * ratio, drag.grab.y * ratio);
    let grab = if out {
        PointDevice::new(
            anchor(grab.x.round(), f64::from(size.width)),
            anchor(grab.y.round(), f64::from(size.height)),
        )
    } else {
        grab
    };
    // v0-a knows full display bounds. Platform placement later clamps to the work area;
    // readiness uses the actual placed origin plus this content anchor.
    let x = (p.x - grab.x).clamp(
        0.0,
        (f64::from(geometry.pixel_size.width) - f64::from(size.width)).max(0.0),
    );
    let y = (p.y - grab.y).clamp(
        0.0,
        (f64::from(geometry.pixel_size.height) - f64::from(size.height)).max(0.0),
    );
    Commit {
        kind: drag.offer.kind,
        peer: drag.offer.peer,
        place: ProxyPlacement {
            display: drag.entry.display,
            x: x.round() as i32,
            y: y.round() as i32,
            drag: out,
        },
        token,
        anchor: (grab.x.round() as i32, grab.y.round() as i32),
        size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_types::geom::{PointLogical, SizeMm};
    use crosspane_types::id::{DisplayId, NodeId};
    fn geometry() -> DisplayGeometry {
        DisplayGeometry {
            physical_size: SizeMm::new(100.0, 100.0),
            pixel_size: PixelSize::new(1200, 900),
            scale: 1.5,
            logical_origin: PointLogical::zero(),
        }
    }
    #[test]
    fn gesture_identity_excludes_geometry_but_includes_window_kind_and_peer() {
        let offer = Offer {
            window: WindowId(1),
            kind: Kind::Out(WindowId(1)),
            peer: NodeId([2; 32]),
            size: PixelSize::new(320, 200),
            scale: 1.0,
        };
        assert!(offer.same_gesture(Offer {
            size: PixelSize::new(600, 700),
            scale: 2.0,
            ..offer
        }));
        for changed in [
            Offer {
                window: WindowId(2),
                ..offer
            },
            Offer {
                kind: Kind::Back(ProjectionKey {
                    source: offer.peer,
                    projection: crosspane_types::id::ProjectionId(1),
                }),
                ..offer
            },
            Offer {
                peer: NodeId([3; 32]),
                ..offer
            },
        ] {
            assert!(!offer.same_gesture(changed));
        }
    }
    #[test]
    fn every_edge_uses_perpendicular_logical_distance() {
        for (edge, point) in [
            (Edge::Right, PointDevice::new(72.0, 700.0)),
            (Edge::Left, PointDevice::new(1128.0, 700.0)),
            (Edge::Bottom, PointDevice::new(700.0, 72.0)),
            (Edge::Top, PointDevice::new(700.0, 828.0)),
        ] {
            assert_eq!(inward(edge, point, geometry()), 48.0);
        }
    }
    #[test]
    fn scaled_titlebar_anchor_and_tiny_content_stay_inside_full_bounds() {
        let peer = NodeId([2; 32]);
        let mut drag = Drag {
            portal: crosspane_platform::PortalId(1),
            offer: Offer {
                window: WindowId(1),
                kind: Kind::Out(WindowId(1)),
                peer,
                size: PixelSize::new(320, 200),
                scale: 1.0,
            },
            grab: PointDevice::new(20.0, -12.0),
            edge: Edge::Right,
            entry: GlobalDisplayId {
                node: peer,
                display: DisplayId(1),
            },
            stage: Stage::Pending,
            motion: VectorMm::zero(),
        };
        let placed = placement(drag, PointDevice::new(1199.0, 899.0), geometry(), 7);
        assert_eq!(placed.anchor, (30, 8));
        assert_eq!(placed.size, PixelSize::new(480, 300));
        assert_eq!((placed.place.x, placed.place.y), (720, 600));
        drag.offer.size = PixelSize::new(2, 2);
        let placed = placement(drag, PointDevice::zero(), geometry(), 8);
        assert!(placed.anchor.0 >= 0 && placed.anchor.0 < placed.size.width as i32);
        assert!(placed.anchor.1 >= 0 && placed.anchor.1 < placed.size.height as i32);
    }
}
