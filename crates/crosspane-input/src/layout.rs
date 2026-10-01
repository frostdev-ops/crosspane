//! Display placement, edge portals and pointer motion on the millimetre canvas.

use crosspane_platform::CapturePortal;
use crosspane_types::geom::{DisplayGeometry, PointDevice, PointMm, RectMm, VectorMm};
use crosspane_types::id::{GlobalDisplayId, NodeId};

use crate::{Edge, PortalId};

/// Displays whose edges are within this distance are touching.
pub const TOUCH_TOLERANCE_MM: f64 = 2.0;
/// Shared edge stretches shorter than this don't form portals.
pub const MIN_PORTAL_MM: f64 = 10.0;
/// Inward normal distance required before an entry edge can cross back.
pub const REARM_MM: f64 = 1.5;

/// One display placed on the shared canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub id: GlobalDisplayId,
    pub geometry: DisplayGeometry,
    /// Top-left corner on the canvas.
    pub origin: PointMm,
}

impl Placed {
    /// `origin` plus `geometry.physical_size`.
    pub fn rect(&self) -> RectMm {
        RectMm::new(self.origin, self.geometry.physical_size)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutOptions {
    /// Millimetres cut off each end of every portal ("dead corners", 03 §3). Default 0.
    pub dead_corner_mm: f64,
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum LayoutError {
    #[error("display {0:?} has invalid geometry")]
    InvalidGeometry(GlobalDisplayId),
    #[error("display {0:?} appears twice")]
    Duplicate(GlobalDisplayId),
    /// The interiors overlap by more than TOUCH_TOLERANCE_MM in both axes.
    #[error("displays {0:?} and {1:?} overlap")]
    Overlap(GlobalDisplayId, GlobalDisplayId),
}

/// A stretch of a display edge where the pointer passes to a display of another node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Portal {
    pub id: PortalId,
    /// The display the pointer leaves, and through which side.
    pub from: GlobalDisplayId,
    pub edge: Edge,
    /// The stretch along that edge in device pixels of `from`, measured from its top (Left/Right
    /// edges) or left (Top/Bottom edges) corner; `start < end`. Same convention as `CapturePortal`.
    pub start: f64,
    pub end: f64,
    /// The display the pointer enters.
    pub to: GlobalDisplayId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    displays: Vec<Placed>,
    portals: Vec<Portal>,
}

impl Layout {
    pub fn new(displays: Vec<Placed>, options: LayoutOptions) -> Result<Layout, LayoutError> {
        for (index, display) in displays.iter().enumerate() {
            let max = display.rect().max();
            if !display.geometry.is_valid()
                || !display.origin.x.is_finite()
                || !display.origin.y.is_finite()
                || !max.x.is_finite()
                || !max.y.is_finite()
            {
                return Err(LayoutError::InvalidGeometry(display.id));
            }
            for other in &displays[..index] {
                if display.id == other.id {
                    return Err(LayoutError::Duplicate(display.id));
                }
                let a = other.rect();
                let b = display.rect();
                if a.max().x.min(b.max().x) - a.min().x.max(b.min().x) > TOUCH_TOLERANCE_MM
                    && a.max().y.min(b.max().y) - a.min().y.max(b.min().y) > TOUCH_TOLERANCE_MM
                {
                    return Err(LayoutError::Overlap(other.id, display.id));
                }
            }
        }

        let mut portals = Vec::new();
        for (index, from) in displays.iter().enumerate() {
            for to in &displays[index + 1..] {
                if from.id.node == to.id.node {
                    continue;
                }
                for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
                    if let Some((start, end)) = touching_span(from, to, edge) {
                        let start = start + options.dead_corner_mm;
                        let end = end - options.dead_corner_mm;
                        if end - start >= MIN_PORTAL_MM {
                            portals.push(make_portal(from, to, edge, start, end));
                            portals.push(make_portal(to, from, opposite(edge), start, end));
                        }
                    }
                }
            }
        }
        portals.sort_by(|a, b| {
            a.from
                .cmp(&b.from)
                .then_with(|| (a.edge as u8).cmp(&(b.edge as u8)))
                .then_with(|| a.start.total_cmp(&b.start))
                .then_with(|| a.to.cmp(&b.to))
        });
        for (portal, id) in portals.iter_mut().zip(1_u32..) {
            portal.id = PortalId(id);
        }
        Ok(Self { displays, portals })
    }

    pub fn displays(&self) -> &[Placed] {
        &self.displays
    }

    pub fn get(&self, id: GlobalDisplayId) -> Option<&Placed> {
        self.displays.iter().find(|display| display.id == id)
    }

    /// Every portal: a pair per shared stretch, one in each direction.
    pub fn portals(&self) -> &[Portal] {
        &self.portals
    }

    /// The portals leaving displays of `node`, as that node's `InputCapture::set_portals` needs
    /// them (`display` = `from.display`).
    pub fn capture_portals(&self, node: NodeId) -> Vec<CapturePortal> {
        self.portals
            .iter()
            .filter(|portal| portal.from.node == node)
            .map(|portal| CapturePortal {
                id: portal.id,
                display: portal.from.display,
                edge: portal.edge,
                from: portal.start,
                to: portal.end,
            })
            .collect()
    }

    /// A display-local device point → canvas.
    pub fn to_canvas(&self, id: GlobalDisplayId, p: PointDevice) -> Option<PointMm> {
        let display = self.get(id)?;
        Some(display.origin + display.geometry.device_to_mm(p).to_vector())
    }

    /// The display containing canvas point `p` (rectangles are half-open: min inclusive, max
    /// exclusive) and the display-local device point there.
    pub fn locate(&self, p: PointMm) -> Option<(GlobalDisplayId, PointDevice)> {
        let display = self
            .displays
            .iter()
            .find(|display| display.rect().contains(p))?;
        Some((display.id, local_device(display, p)))
    }

    /// Where the pointer enters when it leaves through `portal` at `position` (0.0 = `start`, 1.0
    /// = `end`; clamped): the same canvas point along the shared edge, as a device point just
    /// inside `to` (at most 1 device pixel from its edge, clamped with `clamp_device`).
    pub fn entry(&self, portal: PortalId, position: f64) -> Option<(GlobalDisplayId, PointDevice)> {
        let portal = self
            .portals
            .iter()
            .find(|candidate| candidate.id == portal)?;
        let from = self.get(portal.from)?;
        let to = self.get(portal.to)?;
        // Map NaN to the start before clamping to the finite, ordered unit interval.
        let position = if position.is_nan() { 0.0 } else { position };
        let along_device = portal.start + (portal.end - portal.start) * position.clamp(0.0, 1.0);
        let along_canvas =
            span_origin(from, portal.edge) + along_device / density(from, portal.edge);
        Some((to.id, edge_entry(to, opposite(portal.edge), along_canvas)))
    }
}

fn opposite(edge: Edge) -> Edge {
    match edge {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
    }
}

fn vertical(edge: Edge) -> bool {
    matches!(edge, Edge::Left | Edge::Right)
}

fn span_origin(display: &Placed, edge: Edge) -> f64 {
    if vertical(edge) {
        display.origin.y
    } else {
        display.origin.x
    }
}

fn density(display: &Placed, edge: Edge) -> f64 {
    let (x, y) = display.geometry.pixels_per_mm();
    if vertical(edge) { y } else { x }
}

fn along(point: PointMm, edge: Edge) -> f64 {
    if vertical(edge) { point.y } else { point.x }
}

/// The overlap's coordinates on the canvas, before dead corners are removed.
fn touching_span(from: &Placed, to: &Placed, edge: Edge) -> Option<(f64, f64)> {
    let a = from.rect();
    let b = to.rect();
    let distance = match edge {
        Edge::Left => a.min().x - b.max().x,
        Edge::Right => a.max().x - b.min().x,
        Edge::Top => a.min().y - b.max().y,
        Edge::Bottom => a.max().y - b.min().y,
    };
    if distance.abs() > TOUCH_TOLERANCE_MM {
        return None;
    }
    let (start, end) = if vertical(edge) {
        (a.min().y.max(b.min().y), a.max().y.min(b.max().y))
    } else {
        (a.min().x.max(b.min().x), a.max().x.min(b.max().x))
    };
    (start < end).then_some((start, end))
}

fn make_portal(from: &Placed, to: &Placed, edge: Edge, start: f64, end: f64) -> Portal {
    let origin = span_origin(from, edge);
    let density = density(from, edge);
    let length = if vertical(edge) {
        from.geometry.pixel_size.height
    } else {
        from.geometry.pixel_size.width
    };
    Portal {
        id: PortalId(0),
        from: from.id,
        edge,
        start: ((start - origin) * density).max(0.0),
        end: ((end - origin) * density).min(f64::from(length)),
        to: to.id,
    }
}

fn local_device(display: &Placed, point: PointMm) -> PointDevice {
    display
        .geometry
        .mm_to_device((point - display.origin).to_point())
}

fn edge_point(display: &Placed, edge: Edge, coordinate: f64) -> PointMm {
    let rect = display.rect();
    match edge {
        Edge::Left => PointMm::new(rect.min().x, coordinate),
        Edge::Right => PointMm::new(rect.max().x, coordinate),
        Edge::Top => PointMm::new(coordinate, rect.min().y),
        Edge::Bottom => PointMm::new(coordinate, rect.max().y),
    }
}

fn edge_entry(display: &Placed, edge: Edge, coordinate: f64) -> PointDevice {
    display
        .geometry
        .clamp_device(local_device(display, edge_point(display, edge, coordinate)))
}

/// What a pointer step did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    /// The pointer is on `display`, which belongs to the same node as before (possibly another of
    /// that node's displays).
    On {
        display: GlobalDisplayId,
        position: PointDevice,
    },
    /// The pointer left the node through a portal into `display` of another node.
    Crossed {
        portal: PortalId,
        display: GlobalDisplayId,
        position: PointDevice,
    },
}

/// The controller's model of the remote pointer while it controls another node (03 §3).
#[derive(Clone, Debug, PartialEq)]
pub struct PointerTracker {
    display: GlobalDisplayId,
    position: PointDevice,
    // One flag per current-display edge; all portals on a disarmed edge are blocked.
    disarmed: [bool; 4],
}

impl PointerTracker {
    /// Start at a display-local device point; `None` if the display isn't in `layout`.
    pub fn new(layout: &Layout, display: GlobalDisplayId, position: PointDevice) -> Option<Self> {
        let placed = layout.get(display)?;
        let position = placed.geometry.clamp_device(position);
        let point = placed.origin + placed.geometry.device_to_mm(position).to_vector();
        Some(Self {
            display,
            position,
            disarmed: EDGES.map(|edge| normal_distance(placed.rect(), point, edge) <= REARM_MM),
        })
    }

    pub fn position(&self) -> (GlobalDisplayId, PointDevice) {
        (self.display, self.position)
    }

    /// Move by a canvas delta.
    /// - Movement stays on displays: across a gap or past an edge with no neighbour, the pointer is
    ///   clamped at the edge, keeping the other axis's motion (it slides).
    /// - Moving onto a touching display of the **same** node continues there (`On`).
    /// - Leaving through a portal yields `Crossed` with `Layout::entry`'s point; the tracker then
    ///   sits there with its entry edge disarmed until it moves inward by `REARM_MM`.
    /// - A step longer than the display is processed so it can't tunnel past an edge.
    pub fn step(&mut self, layout: &Layout, delta: VectorMm) -> Step {
        let mut remaining = VectorMm::new(
            if delta.x.is_finite() { delta.x } else { 0.0 },
            if delta.y.is_finite() { delta.y } else { 0.0 },
        );
        let Some(mut display) = layout.get(self.display) else {
            return self.on();
        };
        let mut point = display.origin + display.geometry.device_to_mm(self.position).to_vector();
        loop {
            self.rearm(display.rect(), point);
            let Some((fraction, edge)) = first_edge(display.rect(), point, remaining) else {
                point += remaining;
                self.position = display.geometry.clamp_device(local_device(display, point));
                self.rearm(
                    display.rect(),
                    display.origin + display.geometry.device_to_mm(self.position).to_vector(),
                );
                return self.on();
            };
            point += remaining * fraction;
            self.rearm(display.rect(), point);
            remaining *= 1.0 - fraction;
            let coordinate = along(point, edge);

            // A same-node neighbour needs no portal or minimum shared stretch.
            if let Some(next) = layout.displays.iter().find(|next| {
                next.id != display.id
                    && next.id.node == display.id.node
                    && touching_span(display, next, edge)
                        .is_some_and(|(start, end)| coordinate >= start && coordinate < end)
            }) {
                self.display = next.id;
                self.disarmed = [false; 4];
                display = next;
                // Keep the geometric boundary until the remaining motion has been consumed;
                // clamping at every same-node transition would lose a pixel on reverse motion.
                point = edge_point(display, opposite(edge), coordinate);
                continue;
            }

            let coordinate_device =
                (coordinate - span_origin(display, edge)) * density(display, edge);
            if let Some(portal) = layout.portals.iter().find(|portal| {
                !self.disarmed[edge_index(edge)]
                    && portal.from == display.id
                    && portal.edge == edge
                    && coordinate_device >= portal.start
                    && coordinate_device <= portal.end
            }) {
                let fraction = (coordinate_device - portal.start) / (portal.end - portal.start);
                if let Some((target, position)) = layout.entry(portal.id, fraction) {
                    self.display = target;
                    self.position = position;
                    self.disarmed = [false; 4];
                    self.disarmed[edge_index(opposite(edge))] = true;
                    return Step::Crossed {
                        portal: portal.id,
                        display: target,
                        position,
                    };
                }
            }

            // Consume the blocked normal motion, retaining the tangent to slide along the edge.
            if vertical(edge) {
                remaining.x = 0.0;
            } else {
                remaining.y = 0.0;
            }
        }
    }

    fn rearm(&mut self, rect: RectMm, point: PointMm) {
        for edge in EDGES {
            if normal_distance(rect, point, edge) >= REARM_MM {
                self.disarmed[edge_index(edge)] = false;
            }
        }
    }

    fn on(&self) -> Step {
        Step::On {
            display: self.display,
            position: self.position,
        }
    }
}

const EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

fn edge_index(edge: Edge) -> usize {
    match edge {
        Edge::Left => 0,
        Edge::Right => 1,
        Edge::Top => 2,
        Edge::Bottom => 3,
    }
}

fn normal_distance(rect: RectMm, point: PointMm, edge: Edge) -> f64 {
    match edge {
        Edge::Left => point.x - rect.min().x,
        Edge::Right => rect.max().x - point.x,
        Edge::Top => point.y - rect.min().y,
        Edge::Bottom => rect.max().y - point.y,
    }
}

/// Earliest boundary reached by the segment, with horizontal motion winning a corner tie.
fn first_edge(rect: RectMm, point: PointMm, delta: VectorMm) -> Option<(f64, Edge)> {
    let x = if delta.x > 0.0 {
        Some(((rect.max().x - point.x) / delta.x, Edge::Right))
    } else if delta.x < 0.0 {
        Some(((rect.min().x - point.x) / delta.x, Edge::Left))
    } else {
        None
    };
    let y = if delta.y > 0.0 {
        Some(((rect.max().y - point.y) / delta.y, Edge::Bottom))
    } else if delta.y < 0.0 {
        Some(((rect.min().y - point.y) / delta.y, Edge::Top))
    } else {
        None
    };
    [x, y]
        .into_iter()
        .flatten()
        .filter(|(fraction, _)| *fraction >= 0.0 && *fraction <= 1.0)
        .min_by(|a, b| a.0.total_cmp(&b.0))
}
