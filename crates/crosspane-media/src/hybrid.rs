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

use crosspane_types::geom::PixelSize;

use crate::wire::{TILE, VideoRegion};

/// A rectangle of whole 64×64 tiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl TileRect {
    pub fn contains(&self, tx: u32, ty: u32) -> bool {
        tx >= self.x && ty >= self.y && tx - self.x < self.width && ty - self.y < self.height
    }

    /// The pixels it covers in a frame of `size`, clipped at the frame's edge.
    pub fn region(&self, size: PixelSize) -> VideoRegion {
        let x = self.x.saturating_mul(TILE).min(size.width);
        let y = self.y.saturating_mul(TILE).min(size.height);
        VideoRegion {
            x,
            y,
            width: self
                .x
                .saturating_add(self.width)
                .saturating_mul(TILE)
                .min(size.width)
                - x,
            height: self
                .y
                .saturating_add(self.height)
                .saturating_mul(TILE)
                .min(size.height)
                - y,
        }
    }

    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            width: (self.x + self.width).max(other.x + other.width) - x,
            height: (self.y + self.height).max(other.y + other.height) - y,
        }
    }
}

/// Thresholds for region video. The defaults suit desktop content.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegionConfig {
    /// A tile is busy after changing in this many captures without a quiet gap of `exit_after`.
    pub enter_frames: u32,
    /// Video starts when the busy tiles' bounding box covers at least this many tiles.
    pub min_tiles: u32,
    /// Busy tiles quiet this long stop being busy; with none left, video ends.
    pub exit_after: Duration,
    /// The region shrinks to a smaller box only after it has fitted continuously this long.
    pub shrink_after: Duration,
    /// After the video codec failed, stay on tiles this long.
    pub retry_after: Duration,
}

impl Default for RegionConfig {
    fn default() -> Self {
        Self {
            enter_frames: 3,
            min_tiles: 6,
            exit_after: Duration::from_millis(400),
            shrink_after: Duration::from_secs(1),
            retry_after: Duration::from_secs(10),
        }
    }
}

/// What to send for one capture with region video.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionPlan {
    /// Lossless tiles only: the changed and stale tiles, or the tile encoder's key frame.
    Tiles,
    /// `region` as video, plus lossless tiles for the changes outside it. `key` when the region
    /// is new or its size changed (the encoder restarts at the new size with an IDR).
    Video { region: TileRect, key: bool },
}

#[derive(Debug, Default, Clone)]
struct BusyTile {
    changes: u32,
    last_change: Option<Duration>,
}

/// Chooses region video, frame by frame, for one projection. Deterministic: time comes from the
/// caller.
#[derive(Debug)]
pub struct RegionScheduler {
    config: RegionConfig,
    grid: (u32, u32),
    tiles: Vec<BusyTile>,
    region: Option<TileRect>,
    // All candidates during this interval must fit the accumulated smaller rectangle.
    shrink: Option<(TileRect, Duration)>,
    retry_since: Option<Duration>,
    last_now: Duration,
}

impl RegionScheduler {
    pub fn new(config: RegionConfig) -> RegionScheduler {
        Self {
            config,
            grid: (0, 0),
            tiles: Vec::new(),
            region: None,
            shrink: None,
            retry_since: None,
            last_now: Duration::ZERO,
        }
    }

    /// Plan the capture at `now` whose changed tiles are `changed_bits` (a `tiles_x` × `tiles_y`
    /// row-major bitmap, bit `i % 32` of word `i / 32`). `video_available` false → always `Tiles`
    /// (and any region ends). A grid-size change resets all state.
    pub fn plan(
        &mut self,
        tiles_x: u32,
        tiles_y: u32,
        changed_bits: &[u32],
        now: Duration,
        video_available: bool,
    ) -> RegionPlan {
        if self.grid != (tiles_x, tiles_y) {
            self.grid = (tiles_x, tiles_y);
            self.tiles.clear();
            self.region = None;
            self.shrink = None;
            self.retry_since = None;
            self.last_now = Duration::ZERO;
        }
        self.last_now = self.last_now.max(now);
        let now = self.last_now;
        // The wire format caps each frame dimension at 16384 pixels (256 tiles).
        // Invalid bitmaps/grids conservatively end video without allocating unbounded state.
        if tiles_x == 0 || tiles_y == 0 || tiles_x > 256 || tiles_y > 256 {
            return self.end_region();
        }
        let total = tiles_x * tiles_y;
        if changed_bits.len() != total.div_ceil(32) as usize
            || (!total.is_multiple_of(32)
                && changed_bits
                    .last()
                    .is_some_and(|word| word >> (total % 32) != 0))
        {
            self.tiles.clear();
            return self.end_region();
        }
        self.tiles.resize(total as usize, BusyTile::default());
        let mut busy: Option<TileRect> = None;
        for (i, tile) in self.tiles.iter_mut().enumerate() {
            if tile
                .last_change
                .is_some_and(|last| now.saturating_sub(last) >= self.config.exit_after)
            {
                *tile = BusyTile::default();
            }
            if changed_bits[i / 32] & (1 << (i % 32)) != 0 {
                tile.changes = tile.changes.saturating_add(1);
                tile.last_change = Some(now);
            }
            if tile.last_change.is_some() && tile.changes >= self.config.enter_frames {
                let point = TileRect {
                    x: i as u32 % tiles_x,
                    y: i as u32 / tiles_x,
                    width: 1,
                    height: 1,
                };
                busy = Some(busy.map_or(point, |rect| rect.union(point)));
            }
        }
        if !video_available {
            return self.end_region();
        }
        if let Some(failed) = self.retry_since {
            if now.saturating_sub(failed) < self.config.retry_after {
                return self.end_region();
            }
            self.retry_since = None;
        }
        let Some(busy) = busy else {
            return self.end_region();
        };
        let x = busy.x.saturating_sub(1);
        let y = busy.y.saturating_sub(1);
        let candidate = TileRect {
            x,
            y,
            width: (busy.x + busy.width + 1).min(tiles_x) - x,
            height: (busy.y + busy.height + 1).min(tiles_y) - y,
        };
        let Some(current) = self.region else {
            if candidate.width * candidate.height < self.config.min_tiles {
                return RegionPlan::Tiles;
            }
            self.region = Some(candidate);
            self.shrink = None;
            return RegionPlan::Video {
                region: candidate,
                key: true,
            };
        };
        let union = current.union(candidate);
        let mut region = current;
        if union != current {
            region = union;
            self.shrink = None;
        } else if candidate != current {
            let (box_so_far, since) = self.shrink.unwrap_or((candidate, now));
            let fitted = box_so_far.union(candidate);
            if fitted == current {
                // Start a fresh interval; the old union no longer proves a smaller box.
                self.shrink = Some((candidate, now));
            } else if now.saturating_sub(since) >= self.config.shrink_after {
                region = fitted;
                self.shrink = None;
            } else {
                self.shrink = Some((fitted, since));
            }
        } else {
            self.shrink = None;
        }
        let key = (region.width, region.height) != (current.width, current.height);
        self.region = Some(region);
        RegionPlan::Video { region, key }
    }

    /// The video encoder failed: no region (its tiles are already stale) and no video until
    /// `retry_after` has passed.
    pub fn video_failed(&mut self, now: Duration) {
        self.last_now = self.last_now.max(now);
        self.retry_since = Some(self.last_now);
        self.end_region();
    }

    pub fn region(&self) -> Option<TileRect> {
        self.region
    }

    fn end_region(&mut self) -> RegionPlan {
        self.region = None;
        self.shrink = None;
        RegionPlan::Tiles
    }
}
