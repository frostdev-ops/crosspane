//! OS-free newest-frame pacing, target-local crop and terminal-state mapping.

use crosspane_platform::{PlatformError, StreamEndReason};
use crosspane_types::{
    geom::{PixelRect, PixelSize},
    time::MonoTime,
};
use std::time::Duration;

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
