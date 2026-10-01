//! The hybrid scheduler (03 §7.1): per captured frame, lossless tiles or H.264 video.
//!
//! Still content goes as lossless tiles, so text is bit-exact at rest. When a large part of the
//! window keeps changing (video, scrolling, animation) the projection switches to video; when the
//! motion stops it sends one lossless key frame, which makes the destination bit-exact again, and
//! carries on with tiles. Deterministic: time comes from the caller.

use core::time::Duration;

/// Thresholds. The defaults suit desktop content.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HybridConfig {
    /// Enter video when at least this fraction of the tiles changed …
    pub enter_fraction: f32,
    /// … in this many consecutive frames.
    pub enter_frames: u32,
    /// Leave video when at most this fraction changed …
    pub exit_fraction: f32,
    /// … continuously for this long.
    pub exit_after: Duration,
    /// After the video codec failed, stay on tiles this long before trying video again.
    pub retry_after: Duration,
}

impl Default for HybridConfig {
    fn default() -> Self {
        HybridConfig {
            enter_fraction: 0.25,
            enter_frames: 3,
            exit_fraction: 0.02,
            exit_after: Duration::from_millis(400),
            retry_after: Duration::from_secs(10),
        }
    }
}

/// What to send for the frame just captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FramePlan {
    /// The lossless tile delta (or the tile key frame if the tile encoder makes one).
    Tiles,
    /// A video frame; `key` when it is the first after switching to video (an IDR).
    Video { key: bool },
    /// Motion stopped: a lossless **key** frame, then tiles again.
    TilesKey,
}

/// Chooses between tiles and video, frame by frame, for one projection.
#[derive(Debug)]
pub struct HybridScheduler {
    config: HybridConfig,
}

impl HybridScheduler {
    pub fn new(config: HybridConfig) -> HybridScheduler {
        HybridScheduler { config }
    }

    /// Plan the frame captured at `now` (any monotonic clock) in which `changed` of `total` tiles
    /// differ from the previous capture. `video_available` is false when this node has no
    /// encoder or the peer can't decode: then the answer is always `Tiles`.
    pub fn plan(
        &mut self,
        changed: u32,
        total: u32,
        now: Duration,
        video_available: bool,
    ) -> FramePlan {
        // WP-2.14a implements this.
        let _ = (changed, total, now, video_available, &self.config);
        FramePlan::Tiles
    }

    /// The video encoder failed at `now`: back to tiles (the next plan is `TilesKey` if video was
    /// on), and no video until `retry_after` has passed.
    pub fn video_failed(&mut self, now: Duration) {
        let _ = now;
    }

    /// True while the projection is in video mode.
    pub fn in_video(&self) -> bool {
        false
    }
}
