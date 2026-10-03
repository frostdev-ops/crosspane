//! `[E]` PMv2 inputs are physical rectangles and effective DPI; no native acquisition.
//! <https://learn.microsoft.com/en-us/windows/win32/hidpi/high-dpi-desktop-application-development-on-windows>
//! `[U]` Device-path reboot stability, EDID acquisition and HMONITOR lifetime await W1/P9.
//! `[P9f]` The adapter supplies rotation; rcMonitor is already oriented.
//! `[P]` BFS preserves walked seams; others are best-effort. Disconnected roots use primary-scale
//! physical offsets. Any logical overlap refuses the complete layout.

use std::collections::{BTreeMap, BTreeSet};

use crosspane_types::{
    color::ColorSpace,
    display::DisplayInfo,
    geom::{DisplayGeometry, PixelSize, PointDevice, PointLogical, SizeMm},
    id::DisplayId,
};

/// `[E]` Effective DPI includes user scaling; X and Y agree.
/// <https://learn.microsoft.com/en-us/windows/win32/api/shellscalingapi/ne-shellscalingapi-monitor_dpi_type>
pub const MDT_EFFECTIVE_DPI: u32 = 0;
/// `[E]` Scale is DPI / 96.
/// <https://learn.microsoft.com/en-us/windows/win32/hidpi/wm-dpichanged>
pub const USER_DEFAULT_SCREEN_DPI: u32 = 96;
/// `[E]` Carries equal X/Y DPI and a suggested rectangle; acquisition/handling belongs to W1.
/// <https://learn.microsoft.com/en-us/windows/win32/hidpi/wm-dpichanged>
pub const WM_DPICHANGED: u32 = 0x02e0;

/// `[E]` Rectangles use physical virtual-screen coordinates (negative coordinates allowed).
/// <https://learn.microsoft.com/en-us/windows/win32/gdi/the-virtual-screen>
/// `[E]` name corresponds to MONITORINFOEXW.szDevice; device_path to monitorDevicePath:
/// <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-monitorinfoexw>
/// <https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-displayconfig_target_device_name>
/// `[U]` Native observation freshness is the adapter's responsibility. `[P9f]` quarter_turns is 0..3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorProbe {
    pub device_path: String,
    pub name: String,
    pub rc_monitor: [i32; 4],
    pub rc_work: [i32; 4],
    pub primary: bool,
    pub dpi: u32,
    pub refresh_millihz: u32,
    pub edid: Option<Vec<u8>>,
    pub twin: bool,
    pub quarter_turns: u8,
}

/// `[P]` Invalid observations refuse modeling; overlap is the only topology refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryError {
    InvalidMonitor,
    InvalidPrimary,
    DuplicateId,
    DuplicateDevicePath,
    IdSpaceExhausted,
    Overlap,
}

/// `[P]` Direction from the first display in a seam record to the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeamSide {
    Right,
    Left,
    Below,
    Above,
}

/// `[P]` IDs are ordered; kept=false identifies a broken physical seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeamRecord {
    pub displays: (DisplayId, DisplayId),
    pub side: SeamSide,
    pub walked: bool,
    pub kept: bool,
}

/// `[P]` Original positive-length physical seams, sorted by display-ID pair.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeamDiagnostics {
    pub records: Vec<SeamRecord>,
}

/// `[P]` Origins remain in input order; diagnostics describe Crosspane's modeled desktop.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogicalLayout {
    pub origins: Vec<PointLogical>,
    pub seams: SeamDiagnostics,
}

/// `[P]` Displays are sorted by device path, making collision resolution enumeration-independent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DisplayLayout {
    pub displays: Vec<DisplayInfo>,
    pub seams: SeamDiagnostics,
}

/// `[P]` The caller retains this allocator across observations, removals and re-adds.
/// `[U]` A connected monitor must keep the same supplied device path; native stability awaits P9.
#[derive(Clone, Debug, Default)]
pub struct DisplayIds {
    paths: BTreeMap<String, DisplayId>,
    taken: BTreeSet<DisplayId>,
}

impl DisplayIds {
    /// `[P]` Retains exact-path assignments; historical IDs are never reassigned to another path.
    pub fn assign(&mut self, device_path: &str) -> Result<DisplayId, GeometryError> {
        if let Some(&id) = self.paths.get(device_path) {
            return Ok(id);
        }
        let id = display_id(device_path, &self.taken)?;
        self.paths.insert(device_path.into(), id);
        self.taken.insert(id);
        Ok(id)
    }
}

/// `[U]` Hashes the supplied exact path, without claiming reboot stability. FNV-1a 32, then
/// wrapping collision bumps; the caller retains taken IDs for the current enumeration.
pub fn display_id(
    device_path: &str,
    taken: &BTreeSet<DisplayId>,
) -> Result<DisplayId, GeometryError> {
    let mut id = device_path.bytes().fold(0x811c9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x01000193)
    });
    for _ in 0..=taken.len() {
        if !taken.contains(&DisplayId(id)) {
            return Ok(DisplayId(id));
        }
        id = id.wrapping_add(1);
    }
    Err(GeometryError::IdSpaceExhausted)
}

/// `[E]` VESA E-EDID A2 §§3.3, 3.6.2, 3.10.2: checked base block, DTD millimetres
/// first, then nonzero centimetres at 0x15/0x16. Extensions are outside this model.
/// <https://glenwing.github.io/docs/VESA-EEDID-A2.pdf>
/// `[P]` DTD image size is treated as panel size per the model contract; W1/P9 validates that source.
pub fn edid_physical_size(edid: &[u8]) -> Option<SizeMm> {
    let base = edid.get(..128)?;
    if base[..8] != [0, 255, 255, 255, 255, 255, 255, 0]
        || base[18] != 1
        || base[19] > 4
        || base.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) != 0
    {
        return None;
    }
    for d in base[54..126].as_chunks::<18>().0 {
        if d[0] == 0 && d[1] == 0 {
            continue;
        }
        let w = u16::from(d[12]) | (u16::from(d[14] & 0xf0) << 4);
        let h = u16::from(d[13]) | (u16::from(d[14] & 0x0f) << 8);
        if w > 0 && h > 0 {
            return Some(SizeMm::new(f64::from(w), f64::from(h)));
        }
    }
    (base[21] > 0 && base[22] > 0)
        .then(|| SizeMm::new(f64::from(base[21]) * 10.0, f64::from(base[22]) * 10.0))
}

fn extent(r: [i32; 4]) -> Option<PixelSize> {
    let w = u32::try_from(i64::from(r[2]) - i64::from(r[0])).ok()?;
    let h = u32::try_from(i64::from(r[3]) - i64::from(r[1])).ok()?;
    (w > 0 && h > 0).then(|| PixelSize::new(w, h))
}

fn work_valid(work: [i32; 4], monitor: [i32; 4]) -> bool {
    extent(work).is_some()
        && extent(monitor).is_some()
        && work[0] >= monitor[0]
        && work[1] >= monitor[1]
        && work[2] <= monitor[2]
        && work[3] <= monitor[3]
}

// side 0/1: right/left; 2/3: below/above. Corner-only contact is not a seam.
fn seam(a: [i32; 4], b: [i32; 4]) -> Option<(u8, i64)> {
    let y = i64::from(a[3].min(b[3])) - i64::from(a[1].max(b[1]));
    let x = i64::from(a[2].min(b[2])) - i64::from(a[0].max(b[0]));
    if y > 0 && a[2] == b[0] {
        Some((0, y))
    } else if y > 0 && a[0] == b[2] {
        Some((1, y))
    } else if x > 0 && a[3] == b[1] {
        Some((2, x))
    } else if x > 0 && a[1] == b[3] {
        Some((3, x))
    } else {
        None
    }
}

fn outward(end: f64, size: f64) -> f64 {
    let origin = end - size;
    if origin + size > end {
        origin.next_down()
    } else {
        origin
    }
}

fn neighbour(
    parent: DisplayGeometry,
    a: [i32; 4],
    child: DisplayGeometry,
    b: [i32; 4],
    side: u8,
) -> PointLogical {
    let p = parent.logical_origin;
    let ps = parent.logical_size();
    let cs = child.logical_size();
    let x = f64::from(a[0].max(b[0]));
    let y = f64::from(a[1].max(b[1]));
    let tx = p.x + (x - f64::from(a[0])) / parent.scale - (x - f64::from(b[0])) / child.scale;
    let ty = p.y + (y - f64::from(a[1])) / parent.scale - (y - f64::from(b[1])) / child.scale;
    match side {
        0 => PointLogical::new(p.x + ps.width, ty),
        1 => PointLogical::new(outward(p.x, cs.width), ty),
        2 => PointLogical::new(tx, p.y + ps.height),
        _ => PointLogical::new(tx, outward(p.y, cs.height)),
    }
}

fn kept(a: DisplayGeometry, b: DisplayGeometry, side: u8) -> bool {
    let a = a.logical_bounds();
    let b = b.logical_bounds();
    let operand = |start: f64, size: f64| start.abs().max(size).max(1.0);
    let tx =
        8.0 * f64::EPSILON * operand(a.min_x(), a.size.width).max(operand(b.min_x(), b.size.width));
    let ty = 8.0
        * f64::EPSILON
        * operand(a.min_y(), a.size.height).max(operand(b.min_y(), b.size.height));
    let close = |x: f64, y: f64, tolerance: f64| (x - y).abs() <= tolerance;
    match side {
        0 => close(a.max_x(), b.min_x(), tx) && a.min_y().max(b.min_y()) < a.max_y().min(b.max_y()),
        1 => close(a.min_x(), b.max_x(), tx) && a.min_y().max(b.min_y()) < a.max_y().min(b.max_y()),
        2 => close(a.max_y(), b.min_y(), ty) && a.min_x().max(b.min_x()) < a.max_x().min(b.max_x()),
        _ => close(a.min_y(), b.max_y(), ty) && a.min_x().max(b.min_x()) < a.max_x().min(b.max_x()),
    }
}

/// `[P]` BFS from primary, longest physical seam first then ID. Each walked seam aligns its
/// physical overlap start; normal contact is exact to floating-point precision. Non-tree
/// seams are best-effort. Disconnected roots use primary-scale physical offsets, then BFS.
/// Positive-area logical overlap (without tolerance) refuses the entire layout.
/// Floating-point contact may have an outward rounding gap; diagnostic tolerance accounts
/// for origin and extent operands. It never relaxes the strict overlap check.
pub fn logical_origins(
    rects: &[(DisplayId, [i32; 4], f64)],
    primary: usize,
) -> Result<LogicalLayout, GeometryError> {
    if rects.is_empty() {
        return Ok(LogicalLayout::default());
    }
    let root = rects.get(primary).ok_or(GeometryError::InvalidPrimary)?;
    let mut ids = BTreeSet::new();
    let mut geometry = Vec::new();
    for &(id, r, scale) in rects {
        if !ids.insert(id) {
            return Err(GeometryError::DuplicateId);
        }
        let g = DisplayGeometry {
            pixel_size: extent(r).ok_or(GeometryError::InvalidMonitor)?,
            physical_size: SizeMm::new(1.0, 1.0),
            scale,
            logical_origin: PointLogical::zero(),
        };
        if !g.is_valid() {
            return Err(GeometryError::InvalidMonitor);
        }
        geometry.push(g);
    }
    let mut visited = vec![false; rects.len()];
    let mut roots: Vec<_> = (0..rects.len()).collect();
    roots.sort_by_key(|&i| (i != primary, rects[i].0));
    let mut diagnostics = SeamDiagnostics::default();
    let mut walked = BTreeSet::new();
    for start in roots {
        if visited[start] {
            continue;
        }
        geometry[start].logical_origin = PointLogical::new(
            (f64::from(rects[start].1[0]) - f64::from(root.1[0])) / root.2,
            (f64::from(rects[start].1[1]) - f64::from(root.1[1])) / root.2,
        );
        let mut queue = vec![start];
        visited[start] = true;
        let mut cursor = 0;
        while cursor < queue.len() {
            let parent = queue[cursor];
            cursor += 1;
            let mut neighbours: Vec<_> = (0..rects.len())
                .filter(|&i| !visited[i])
                .filter_map(|i| {
                    seam(rects[parent].1, rects[i].1).map(|(side, length)| (i, side, length))
                })
                .collect();
            neighbours.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| rects[a.0].0.cmp(&rects[b.0].0)));
            for (child, side, _) in neighbours {
                geometry[child].logical_origin = neighbour(
                    geometry[parent],
                    rects[parent].1,
                    geometry[child],
                    rects[child].1,
                    side,
                );
                visited[child] = true;
                queue.push(child);
                walked.insert((parent.min(child), parent.max(child)));
            }
        }
    }
    for (i, a) in geometry.iter().enumerate() {
        let a_bounds = a.logical_bounds();
        if !a.is_valid()
            || !a_bounds.max_x().is_finite()
            || !a_bounds.max_y().is_finite()
            || a_bounds.max_x() <= a_bounds.min_x()
            || a_bounds.max_y() <= a_bounds.min_y()
        {
            return Err(GeometryError::InvalidMonitor);
        }
        for (j, b) in geometry[..i].iter().enumerate() {
            let b_bounds = b.logical_bounds();
            if a_bounds.min_x().max(b_bounds.min_x()) < a_bounds.max_x().min(b_bounds.max_x())
                && a_bounds.min_y().max(b_bounds.min_y()) < a_bounds.max_y().min(b_bounds.max_y())
            {
                return Err(GeometryError::Overlap);
            }
            if let Some((side, _)) = seam(rects[i].1, rects[j].1) {
                let is_kept = kept(*a, *b, side);
                let (displays, side) = if rects[i].0 < rects[j].0 {
                    ((rects[i].0, rects[j].0), side)
                } else {
                    ((rects[j].0, rects[i].0), side ^ 1)
                };
                diagnostics.records.push(SeamRecord {
                    displays,
                    side: [
                        SeamSide::Right,
                        SeamSide::Left,
                        SeamSide::Below,
                        SeamSide::Above,
                    ][side as usize],
                    walked: walked.contains(&(j, i)),
                    kept: is_kept,
                });
            }
        }
    }
    diagnostics.records.sort_by_key(|record| record.displays);
    Ok(LogicalLayout {
        origins: geometry.iter().map(|g| g.logical_origin).collect(),
        seams: diagnostics,
    })
}

/// `[E]` rcMonitor supplies oriented pixel extents; DPI / 96 supplies scale. `[P9f]` Only EDID
/// millimetres rotate; absent/zero size uses oriented pixels at 96 DPI. Twins are excluded.
/// `[U]` Identity/source/rotation facts are supplied by the adapter, not verified here.
pub fn displays(
    probes: &[MonitorProbe],
    ids: &mut DisplayIds,
) -> Result<DisplayLayout, GeometryError> {
    let mut probes: Vec<_> = probes.iter().filter(|p| !p.twin).collect();
    probes.sort_by(|a, b| a.device_path.cmp(&b.device_path));
    if probes.is_empty() {
        return Ok(DisplayLayout::default());
    }
    if probes
        .windows(2)
        .any(|p| p[0].device_path == p[1].device_path)
    {
        return Err(GeometryError::DuplicateDevicePath);
    }
    let primary = probes
        .iter()
        .position(|p| p.primary)
        .ok_or(GeometryError::InvalidPrimary)?;
    if probes[primary + 1..].iter().any(|p| p.primary) {
        return Err(GeometryError::InvalidPrimary);
    }
    let mut updated = ids.clone();
    let mut displays = Vec::new();
    let mut rects = Vec::new();
    for p in probes {
        if p.device_path.is_empty() || p.quarter_turns > 3 || !work_valid(p.rc_work, p.rc_monitor) {
            return Err(GeometryError::InvalidMonitor);
        }
        let id = updated.assign(&p.device_path)?;
        let pixels = extent(p.rc_monitor).ok_or(GeometryError::InvalidMonitor)?;
        let physical = p
            .edid
            .as_deref()
            .and_then(edid_physical_size)
            .map(|mut mm| {
                if p.quarter_turns % 2 == 1 {
                    std::mem::swap(&mut mm.width, &mut mm.height);
                }
                mm
            })
            .unwrap_or_else(|| {
                SizeMm::new(
                    f64::from(pixels.width) * 25.4 / f64::from(USER_DEFAULT_SCREEN_DPI),
                    f64::from(pixels.height) * 25.4 / f64::from(USER_DEFAULT_SCREEN_DPI),
                )
            });
        let scale = f64::from(p.dpi) / f64::from(USER_DEFAULT_SCREEN_DPI);
        rects.push((id, p.rc_monitor, scale));
        displays.push(DisplayInfo {
            id,
            name: p.name.clone(),
            refresh_millihz: p.refresh_millihz,
            color_space: ColorSpace::Srgb,
            hdr: false,
            geometry: DisplayGeometry {
                physical_size: physical,
                pixel_size: pixels,
                scale,
                logical_origin: PointLogical::zero(),
            },
        });
    }
    let layout = logical_origins(&rects, primary)?;
    for (display, origin) in displays.iter_mut().zip(layout.origins) {
        display.geometry.logical_origin = origin;
    }
    *ids = updated;
    Ok(DisplayLayout {
        displays,
        seams: layout.seams,
    })
}

/// `[E]` Physical coordinates may be negative; subtraction produces display-local device pixels.
pub fn physical_to_device(pt: (i32, i32), rc_monitor: [i32; 4]) -> PointDevice {
    PointDevice::new(
        f64::from(pt.0) - f64::from(rc_monitor[0]),
        f64::from(pt.1) - f64::from(rc_monitor[1]),
    )
}

/// `[E]` rcWork is a physical rectangle inside rcMonitor. `[P]` Oversized windows anchor at the
/// work-area top-left; this clamps geometry only and does not perform or promise a native move.
pub fn work_area_clamp(
    origin: PointDevice,
    size: PixelSize,
    rc_work: [i32; 4],
    rc_monitor: [i32; 4],
) -> Result<PointDevice, GeometryError> {
    if !work_valid(rc_work, rc_monitor)
        || !origin.x.is_finite()
        || !origin.y.is_finite()
        || size.width == 0
        || size.height == 0
    {
        return Err(GeometryError::InvalidMonitor);
    }
    let min = physical_to_device((rc_work[0], rc_work[1]), rc_monitor);
    let max = physical_to_device((rc_work[2], rc_work[3]), rc_monitor);
    Ok(PointDevice::new(
        origin
            .x
            .clamp(min.x, (max.x - f64::from(size.width)).max(min.x)),
        origin
            .y
            .clamp(min.y, (max.y - f64::from(size.height)).max(min.y)),
    ))
}
