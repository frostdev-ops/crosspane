//! OS-free newest-frame pacing, target-local crop and terminal-state mapping.

use super::displays::MonitorSnapshot;
use crosspane_platform::{PlatformError, StreamEndReason};
use crosspane_types::{
    geom::{PixelRect, PixelSize},
    id::DisplayId,
    time::MonoTime,
};
use std::{sync::Arc, time::Duration};

/// Fresh coherent monitor facts from the retained owner. WindowsDisplays supplies a
/// weak reader: after the platform drops it, requests fail rather than use cached facts.
pub type MonitorSnapshotReader =
    Arc<dyn Fn() -> Result<MonitorSnapshot, PlatformError> + Send + Sync>;

/// Holds only the newest native frame. Replaced values are returned so their
/// owner can release the OS buffer immediately.
pub struct Latest<T> {
    latest: Option<T>,
    interval: Duration,
    last: Option<MonoTime>,
}

impl<T> std::fmt::Debug for Latest<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Latest")
            .field("pending", &self.latest.is_some())
            .finish_non_exhaustive()
    }
}

impl<T> Latest<T> {
    pub fn new(max_fps: u32) -> Result<Self, PlatformError> {
        if max_fps == 0 {
            return Err(PlatformError::Backend("max_fps must be positive".into()));
        }
        Ok(Self {
            latest: None,
            interval: Duration::from_nanos(1_000_000_000_u64.div_ceil(u64::from(max_fps.max(1)))),
            last: None,
        })
    }
    pub fn push(&mut self, frame: T) -> Option<T> {
        self.latest.replace(frame)
    }
    pub fn due(&self, now: MonoTime) -> bool {
        self.last
            .is_none_or(|last| now >= last.saturating_add(self.interval))
    }
    pub fn take(&mut self, now: MonoTime) -> Option<T> {
        if self.due(now) {
            self.latest.take()
        } else {
            None
        }
    }
    pub fn delivered(&mut self, now: MonoTime) {
        self.last = Some(now);
    }
    pub fn clear(&mut self) -> Option<T> {
        self.latest.take()
    }
}

pub fn validate_crop(crop: Option<PixelRect>) -> Result<(), PlatformError> {
    if crop.is_some_and(|r| r.min.x < 0 || r.min.y < 0 || r.is_empty()) {
        return Err(PlatformError::Backend(
            "crop must be nonempty and non-negative".into(),
        ));
    }
    Ok(())
}

/// Intersect against the current buffer: a resize may arrive before set_crop.
/// An empty intersection skips a frame rather than returning stale pixels.
pub fn crop_rect(size: PixelSize, crop: Option<PixelRect>) -> Option<PixelRect> {
    let width = i32::try_from(size.width).ok()?;
    let height = i32::try_from(size.height).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let full = PixelRect::new((0, 0).into(), (width, height).into());
    crop.map_or(Some(full), |wanted| wanted.intersection(&full))
        .filter(|r| !r.is_empty())
}

/// A growing WGC ContentSize may temporarily exceed the returned pool surface.
/// Never copy outside that surface or deliver a partial stand-in for the requested ROI.
pub fn surface_ready(surface: PixelSize, roi: PixelRect) -> Result<bool, PlatformError> {
    if surface.width == 0 || surface.height == 0 || roi.is_empty() || roi.min.x < 0 || roi.min.y < 0
    {
        return Err(PlatformError::Backend("invalid WGC surface/ROI".into()));
    }
    if roi.max.x as u32 > surface.width || roi.max.y as u32 > surface.height {
        return Ok(false);
    }
    Ok(true)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetState {
    Live,
    Minimized,
    Gone,
}

pub fn end_reason(open: bool, same_epoch: bool, target: TargetState) -> Option<StreamEndReason> {
    if !open || !same_epoch {
        return Some(StreamEndReason::Blocked);
    }
    match target {
        TargetState::Live => None,
        // The window is still alive, but WGC cannot supply it while minimized.
        TargetState::Minimized => Some(StreamEndReason::Failed),
        TargetState::Gone => Some(StreamEndReason::TargetGone),
    }
}

/// A retained display identity plus fresh physical virtual-screen bounds. Geometry is
/// observation-scoped; it is deliberately excluded from the native binding identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorTarget {
    pub id: DisplayId,
    pub handle: usize,
    pub device_path: String,
    pub bounds: PixelRect,
}
impl MonitorTarget {
    pub fn same_binding(&self, other: &Self) -> bool {
        self.id == other.id && self.handle == other.handle && self.device_path == other.device_path
    }
}

/// Resolve only the requested retained ID. Never allocate an ID or choose a primary,
/// nearest or last-known monitor. DisplayInfo.name and MonitorProbe.name are the exact
/// MONITORINFOEX GDI source name committed by the same W1.5c observation. A twin, which is
/// hidden from `displays`, resolves only when no DisplayInfo is published for the same ID.
pub fn resolve_monitor(
    snapshot: &MonitorSnapshot,
    id: DisplayId,
) -> Result<MonitorTarget, PlatformError> {
    let handle = *snapshot.monitors.get(&id).ok_or(PlatformError::NotFound)?;
    let invalid = || PlatformError::Backend("ambiguous capture monitor mapping".into());
    if handle == 0 || snapshot.monitors.values().filter(|h| **h == handle).count() != 1 {
        return Err(invalid());
    }
    let mut displays = snapshot.displays.iter().filter(|d| d.id == id);
    let Some(display) = displays.next() else {
        return resolve_twin(snapshot, id, handle);
    };
    if displays.next().is_some()
        || display.name.is_empty()
        || snapshot
            .displays
            .iter()
            .filter(|d| d.name == display.name)
            .count()
            != 1
    {
        return Err(invalid());
    }
    let mut probes = snapshot.probes.iter().filter(|p| p.name == display.name);
    let probe = probes.next().ok_or_else(invalid)?;
    if probes.next().is_some()
        || probe.device_path.is_empty()
        || snapshot
            .probes
            .iter()
            .filter(|p| p.device_path == probe.device_path)
            .count()
            != 1
    {
        return Err(invalid());
    }
    let [left, top, right, bottom] = probe.rc_monitor;
    let width = i64::from(right) - i64::from(left);
    let height = i64::from(bottom) - i64::from(top);
    if !(1..=i64::from(i32::MAX)).contains(&width)
        || !(1..=i64::from(i32::MAX)).contains(&height)
        || display.geometry.pixel_size != PixelSize::new(width as u32, height as u32)
    {
        return Err(PlatformError::Backend(
            "invalid capture monitor geometry".into(),
        ));
    }
    Ok(MonitorTarget {
        id,
        handle,
        device_path: probe.device_path.clone(),
        bounds: PixelRect::new((left, top).into(), (right, bottom).into()),
    })
}

/// Exactly one twin probe may assign to the ID. Assignment runs on a clone of the allocator, so
/// resolving a twin never reserves or mints an ID; zero or several matches are refused.
fn resolve_twin(
    snapshot: &MonitorSnapshot,
    id: DisplayId,
    handle: usize,
) -> Result<MonitorTarget, PlatformError> {
    let invalid = || PlatformError::Backend("ambiguous capture monitor mapping".into());
    let mut ids = snapshot.ids.clone();
    let mut twins = snapshot
        .probes
        .iter()
        .filter(|p| p.twin && ids.assign(&p.device_path).ok() == Some(id));
    let probe = twins.next().ok_or_else(invalid)?;
    if twins.next().is_some()
        || probe.device_path.is_empty()
        || snapshot
            .probes
            .iter()
            .filter(|p| p.device_path == probe.device_path)
            .count()
            != 1
    {
        return Err(invalid());
    }
    let [left, top, right, bottom] = probe.rc_monitor;
    let width = i64::from(right) - i64::from(left);
    let height = i64::from(bottom) - i64::from(top);
    if !(1..=i64::from(i32::MAX)).contains(&width) || !(1..=i64::from(i32::MAX)).contains(&height) {
        return Err(PlatformError::Backend(
            "invalid capture monitor geometry".into(),
        ));
    }
    Ok(MonitorTarget {
        id,
        handle,
        device_path: probe.device_path.clone(),
        bounds: PixelRect::new((left, top).into(), (right, bottom).into()),
    })
}

/// Fail closed when fresh native facts are unavailable. A missing binding is distinct
/// from a query/WGC failure, and the gate and epoch always take precedence.
pub fn failure_reason(open: bool, same_epoch: bool, error: &PlatformError) -> StreamEndReason {
    if !open || !same_epoch {
        StreamEndReason::Blocked
    } else if matches!(error, PlatformError::NotFound) {
        StreamEndReason::TargetGone
    } else {
        StreamEndReason::Failed
    }
}

/// A replaced native handle/path is a lost capture target even when its retained
/// public ID reconnects. A later start may capture that new binding explicitly.
pub fn refresh_monitor(
    snapshot: &MonitorSnapshot,
    previous: &MonitorTarget,
) -> Result<MonitorTarget, PlatformError> {
    let fresh = resolve_monitor(snapshot, previous.id)?;
    if previous.same_binding(&fresh) {
        Ok(fresh)
    } else {
        Err(PlatformError::NotFound)
    }
}
