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
    video: bool,
    enter_count: u32,
    still_since: Option<Duration>,
    retry_since: Option<Duration>,
    tiles_key_pending: bool,
    last_now: Duration,
}

impl HybridScheduler {
    pub fn new(config: HybridConfig) -> HybridScheduler {
        HybridScheduler {
            config,
            video: false,
            enter_count: 0,
            still_since: None,
            retry_since: None,
            tiles_key_pending: false,
            last_now: Duration::ZERO,
        }
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
        let now = self.monotonic_now(now);
        if total == 0 {
            // No image to encode. Preserve the mode (and any pending key) until there is one.
            self.enter_count = 0;
            self.still_since = None;
            return FramePlan::Tiles;
        }
        if self.tiles_key_pending {
            self.tiles_key_pending = false;
            return FramePlan::TilesKey;
        }
        if !video_available {
            return self.leave_video();
        }
        if let Some(failed_at) = self.retry_since {
            if now.saturating_sub(failed_at) < self.config.retry_after {
                return FramePlan::Tiles;
            }
            self.retry_since = None;
        }

        let fraction = changed as f32 / total as f32;
        if self.video {
            if fraction <= self.config.exit_fraction {
                let still_since = self.still_since.get_or_insert(now);
                if now.saturating_sub(*still_since) >= self.config.exit_after {
                    return self.leave_video();
                }
            } else {
                self.still_since = None;
            }
            FramePlan::Video { key: false }
        } else {
            if fraction >= self.config.enter_fraction {
                self.enter_count = self.enter_count.saturating_add(1);
                if self.enter_count >= self.config.enter_frames {
                    self.video = true;
                    self.enter_count = 0;
                    return FramePlan::Video { key: true };
                }
            } else {
                self.enter_count = 0;
            }
            FramePlan::Tiles
        }
    }

    /// The video encoder failed at `now`: back to tiles (the next plan is `TilesKey` if video was
    /// on), and no video until `retry_after` has passed.
    pub fn video_failed(&mut self, now: Duration) {
        self.retry_since = Some(self.monotonic_now(now));
        self.tiles_key_pending |= self.video;
        self.video = false;
        self.enter_count = 0;
        self.still_since = None;
    }

    /// True while the projection is in video mode.
    pub fn in_video(&self) -> bool {
        self.video
    }

    fn monotonic_now(&mut self, now: Duration) -> Duration {
        self.last_now = self.last_now.max(now);
        self.last_now
    }

    fn leave_video(&mut self) -> FramePlan {
        self.enter_count = 0;
        self.still_since = None;
        if self.video {
            self.video = false;
            FramePlan::TilesKey
        } else {
            FramePlan::Tiles
        }
    }
}
