//! Frame bookkeeping for the ScreenCast capture (WP-G2.2). Pure logic: crop validation and
//! clamping, damage clipping and translation, the damage accumulator behind each delivered frame,
//! the frame-rate pacer, and the timestamps frames carry. Only [`mono_now`] reads the OS clock.

use std::time::{Duration, Instant};

use crosspane_platform::PlatformError;
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use crosspane_types::time::MonoTime;

/// Most rectangles a [`DamageAcc`] keeps before it collapses them to their bounding box.
pub(super) const MAX_RECTS: usize = 32;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Reject a crop without area (max <= min on either axis). Negative origins and rectangles that
/// stick out of the buffer are fine here: they are clamped when a frame arrives.
pub(in crate::portal) fn validate_crop(crop: Option<PixelRect>) -> Result<(), PlatformError> {
    match crop {
        Some(rect) if rect.is_empty() => Err(PlatformError::Backend(
            "screencast: crop must be nonempty".into(),
        )),
        _ => Ok(()),
    }
}

/// `crop` (None = the whole buffer) clamped to a buffer of `size`; None when nothing of it lies
/// inside the buffer, or the size does not fit i32, or the buffer is empty.
pub(super) fn clamp_crop(size: PixelSize, crop: Option<PixelRect>) -> Option<PixelRect> {
    let width = i32::try_from(size.width).ok()?;
    let height = i32::try_from(size.height).ok()?;
    let full = PixelRect::new(point2(0, 0), point2(width, height));
    let rect = crop.map_or(Some(full), |crop| crop.intersection(&full))?;
    (!rect.is_empty()).then_some(rect)
}

/// Clip `rects` to `bounds`, dropping those that end up empty. Order kept.
pub(super) fn clip_damage(rects: &[PixelRect], bounds: PixelRect) -> Vec<PixelRect> {
    rects
        .iter()
        .filter_map(|rect| rect.intersection(&bounds))
        .collect()
}

/// Damage in buffer coordinates -> crop space: intersect each rect with `crop`, subtract
/// `crop.min`, drop empty results. Use i64 intermediates; never overflow/panic.
pub(super) fn translate_damage(damage: &[PixelRect], crop: PixelRect) -> Vec<PixelRect> {
    damage
        .iter()
        .filter_map(|rect| {
            let clipped = rect.intersection(&crop)?;
            let origin_x = i64::from(crop.min.x);
            let origin_y = i64::from(crop.min.y);
            Some(PixelRect::new(
                point2(
                    saturate(i64::from(clipped.min.x) - origin_x),
                    saturate(i64::from(clipped.min.y) - origin_y),
                ),
                point2(
                    saturate(i64::from(clipped.max.x) - origin_x),
                    saturate(i64::from(clipped.max.y) - origin_y),
                ),
            ))
        })
        .collect()
}

/// `value` as an i32, saturated. Only a crop wider than `i32::MAX` can exceed the range, and
/// `clamp_crop` never produces one.
fn saturate(value: i64) -> i32 {
    match i32::try_from(value) {
        Ok(value) => value,
        Err(_) if value < 0 => i32::MIN,
        Err(_) => i32::MAX,
    }
}

/// Damage accumulated over buffers that were consumed but not (yet) delivered, so the next
/// delivered frame reports everything that changed since the last delivered one.
/// Starts UNKNOWN (the first frame, a crop change, a discontinuity, a format change).
#[derive(Debug)]
pub(super) struct DamageAcc {
    /// Something changed that cannot be described; cleared by `take`.
    unknown: bool,
    /// Non-empty rectangles changed since the last `take`, collapsed past `MAX_RECTS`.
    rects: Vec<PixelRect>,
}

impl DamageAcc {
    /// An accumulator whose first `take` returns None.
    pub(super) fn new() -> DamageAcc {
        DamageAcc {
            unknown: true,
            rects: Vec::new(),
        }
    }

    /// Something changed that cannot be described. The next `take` returns None.
    pub(super) fn add_unknown(&mut self) {
        self.unknown = true;
        self.rects.clear();
    }

    /// Add damage already clipped to the buffer. An empty slice is unknown damage. Empty
    /// rectangles are dropped, so a slice of only empty rectangles changes nothing.
    pub(super) fn add(&mut self, rects: &[PixelRect]) {
        if rects.is_empty() {
            self.add_unknown();
            return;
        }
        if self.unknown {
            // The next `take` is None whatever is added now.
            return;
        }
        self.rects
            .extend(rects.iter().filter(|rect| !rect.is_empty()).copied());
        if self.rects.len() > MAX_RECTS {
            self.rects = bounding_box(&self.rects).into_iter().collect();
        }
    }

    /// None if any contribution since the last `take` was unknown; else the union as a list.
    /// More than `MAX_RECTS` rectangles collapse to their single bounding box (a superset is
    /// always valid). Afterwards the accumulator is KNOWN-and-empty, so a later take without any
    /// add returns `Some(vec![])`.
    pub(super) fn take(&mut self) -> Option<Vec<PixelRect>> {
        let unknown = std::mem::replace(&mut self.unknown, false);
        let rects = std::mem::take(&mut self.rects);
        (!unknown).then_some(rects)
    }
}

/// The smallest rectangle containing all of `rects`; None when there are none.
fn bounding_box(rects: &[PixelRect]) -> Option<PixelRect> {
    rects.iter().copied().reduce(|a, b| a.union(&b))
}

/// Rate limiter for delivered frames: at most `max_fps` per second, on a fixed grid when the
/// source is faster, no catching up bursts.
#[derive(Debug)]
pub(super) struct Pacer {
    /// Time between slots: ceil(1e9 / max_fps) ns.
    interval: Duration,
    /// The next slot; None until the first delivery.
    next: Option<Instant>,
}

impl Pacer {
    /// interval = ceil(1e9 / max(max_fps, 1)) ns.
    pub(super) fn new(max_fps: u32) -> Pacer {
        let fps = u64::from(max_fps.max(1));
        Pacer {
            interval: Duration::from_nanos(NANOS_PER_SECOND.div_ceil(fps)),
            next: None,
        }
    }

    /// True when a frame may be delivered at `now` (nothing delivered yet, or now >= next slot).
    pub(super) fn ready(&self, now: Instant) -> bool {
        self.next.is_none_or(|next| now >= next)
    }

    /// The next slot (None = a frame may go at any time).
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.next
    }

    /// Record a delivery at `now`: the next slot is the previous slot + interval when that is
    /// still in the future (delivery was on schedule, possibly a little late), else now + interval
    /// (first delivery, or after an idle gap).
    pub(super) fn delivered(&mut self, now: Instant) {
        let on_grid = self
            .next
            .and_then(|slot| slot.checked_add(self.interval))
            .filter(|slot| *slot > now);
        self.next = on_grid.or_else(|| now.checked_add(self.interval));
    }
}

/// A buffer timestamp on this node's CLOCK_MONOTONIC: the producer's `pts` (nanoseconds) when it
/// is positive, not in the future of `now`, and at most 1 s older; otherwise `now`.
pub(in crate::portal) fn frame_time(pts: Option<i64>, now: MonoTime) -> MonoTime {
    let now_ns = now.as_nanos();
    match pts.and_then(|pts| u64::try_from(pts).ok()) {
        Some(pts) if pts > 0 && pts <= now_ns && now_ns - pts <= NANOS_PER_SECOND => {
            MonoTime::from_nanos(pts)
        }
        _ => now,
    }
}

/// CLOCK_MONOTONIC now, the same clock the Hyprland backend stamps frames with
/// (rustix::time::clock_gettime(ClockId::Monotonic)); never panics (saturating/try_from, 0 on an
/// impossible value).
pub(in crate::portal) fn mono_now() -> MonoTime {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let seconds = u64::try_from(time.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(time.tv_nsec).unwrap_or(0);
    MonoTime::from_nanos(
        seconds
            .saturating_mul(NANOS_PER_SECOND)
            .saturating_add(nanos),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> PixelRect {
        PixelRect::new(point2(x0, y0), point2(x1, y1))
    }

    fn size(width: u32, height: u32) -> PixelSize {
        PixelSize::new(width, height)
    }

    /// A small rectangle that moves along the diagonal, so a union of them is easy to predict.
    fn strip(index: i32) -> PixelRect {
        rect(index * 10, index, index * 10 + 2, index + 2)
    }

    fn assert_nonempty_crop_error(result: Result<(), PlatformError>) {
        match result {
            Err(PlatformError::Backend(message)) => {
                assert_eq!(message, "screencast: crop must be nonempty");
            }
            other => panic!("expected the nonempty-crop error, got {other:?}"),
        }
    }

    #[test]
    fn validate_crop_accepts_none_and_areas_and_rejects_empty_crops() {
        assert!(validate_crop(None).is_ok());
        assert!(validate_crop(Some(rect(0, 0, 10, 10))).is_ok());
        assert!(validate_crop(Some(rect(-50, -20, 10, 10))).is_ok());
        assert_nonempty_crop_error(validate_crop(Some(rect(5, 0, 5, 10))));
        assert_nonempty_crop_error(validate_crop(Some(rect(0, 5, 10, 5))));
        assert_nonempty_crop_error(validate_crop(Some(rect(10, 0, 5, 10))));
        assert_nonempty_crop_error(validate_crop(Some(rect(0, 10, 10, 5))));
    }

    #[test]
    fn clamp_crop_fits_the_crop_to_the_buffer() {
        let buffer = size(100, 50);
        assert_eq!(clamp_crop(buffer, None), Some(rect(0, 0, 100, 50)));
        assert_eq!(
            clamp_crop(buffer, Some(rect(10, 5, 60, 45))),
            Some(rect(10, 5, 60, 45))
        );
        assert_eq!(
            clamp_crop(buffer, Some(rect(-20, -10, 40, 20))),
            Some(rect(0, 0, 40, 20))
        );
        assert_eq!(
            clamp_crop(buffer, Some(rect(90, 40, 200, 200))),
            Some(rect(90, 40, 100, 50))
        );
    }

    #[test]
    fn clamp_crop_is_none_when_nothing_is_inside() {
        let buffer = size(100, 50);
        // Touching the edge from outside is still outside.
        assert_eq!(clamp_crop(buffer, Some(rect(100, 0, 150, 50))), None);
        assert_eq!(clamp_crop(buffer, Some(rect(-30, -30, -1, -1))), None);
        assert_eq!(clamp_crop(size(0, 50), None), None);
        assert_eq!(clamp_crop(size(100, 0), None), None);
        assert_eq!(clamp_crop(size(0, 0), Some(rect(0, 0, 1, 1))), None);
    }

    #[test]
    fn clamp_crop_rejects_sizes_above_i32_max() {
        assert_eq!(clamp_crop(size(2_147_483_648, 10), None), None);
        assert_eq!(clamp_crop(size(u32::MAX, 10), None), None);
        assert_eq!(clamp_crop(size(10, u32::MAX), None), None);
    }

    #[test]
    fn clip_damage_keeps_order_and_drops_what_lies_outside() {
        let bounds = rect(0, 0, 100, 50);
        let damage = [
            rect(-10, -10, 20, 20), // partial: (0, 0, 20, 20)
            rect(200, 0, 300, 50),  // outside: dropped
            rect(90, 40, 120, 80),  // partial: (90, 40, 100, 50)
            rect(5, 5, 5, 9),       // empty: dropped
        ];
        assert_eq!(
            clip_damage(&damage, bounds),
            vec![rect(0, 0, 20, 20), rect(90, 40, 100, 50)]
        );
        assert!(clip_damage(&[], bounds).is_empty());
    }

    #[test]
    fn clip_damage_survives_extreme_values() {
        let everything = rect(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        let damage = [everything, rect(i32::MAX - 1, i32::MIN, i32::MAX, i32::MAX)];
        assert_eq!(clip_damage(&damage, everything), damage.to_vec());
        let corner = rect(i32::MIN, i32::MIN, i32::MIN + 1, i32::MIN + 1);
        assert_eq!(clip_damage(&damage, corner), vec![corner]);
    }

    #[test]
    fn translate_damage_moves_clipped_rects_to_the_crop_origin() {
        let crop = rect(100, 50, 300, 250);
        let damage = [
            rect(0, 0, 150, 60),      // partial top-left: (0, 0, 50, 10)
            rect(120, 80, 140, 90),   // inside: (20, 30, 40, 40)
            rect(250, 200, 400, 400), // partial bottom-right: (150, 150, 200, 200)
            rect(0, 0, 100, 50),      // touches the crop corner only: dropped
            rect(300, 0, 310, 300),   // outside on the right: dropped
        ];
        assert_eq!(
            translate_damage(&damage, crop),
            vec![
                rect(0, 0, 50, 10),
                rect(20, 30, 40, 40),
                rect(150, 150, 200, 200)
            ]
        );
    }

    #[test]
    fn translate_damage_survives_extreme_values() {
        let everything = rect(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        // The crop is wider than i32::MAX, so the far edge saturates.
        assert_eq!(
            translate_damage(&[everything], everything),
            vec![rect(0, 0, i32::MAX, i32::MAX)]
        );
        let high = rect(i32::MAX - 10, i32::MAX - 10, i32::MAX, i32::MAX);
        assert_eq!(
            translate_damage(&[everything], high),
            vec![rect(0, 0, 10, 10)]
        );
        let low = rect(i32::MIN, i32::MIN, -1, -1);
        assert_eq!(
            translate_damage(&[everything], low),
            vec![rect(0, 0, i32::MAX, i32::MAX)]
        );
        let empty = rect(i32::MIN, i32::MIN, i32::MIN, i32::MIN);
        assert!(translate_damage(&[empty], everything).is_empty());
    }

    #[test]
    fn damage_acc_starts_unknown_and_is_known_after_a_take() {
        let mut acc = DamageAcc::new();
        assert_eq!(acc.take(), None);
        assert_eq!(acc.take(), Some(vec![]));
    }

    #[test]
    fn damage_acc_add_before_the_first_take_stays_unknown() {
        // The first delivered frame has no baseline, so damage added first must not clear the start.
        let mut acc = DamageAcc::new();
        acc.add(&[rect(0, 0, 10, 10)]);
        assert_eq!(acc.take(), None);
        assert_eq!(acc.take(), Some(vec![]));
    }

    #[test]
    fn damage_acc_accumulates_rects_in_order_until_taken() {
        let mut acc = DamageAcc::new();
        acc.take();
        acc.add(&[rect(0, 0, 10, 10)]);
        acc.add(&[rect(20, 20, 30, 30), rect(5, 5, 6, 6)]);
        assert_eq!(
            acc.take(),
            Some(vec![
                rect(0, 0, 10, 10),
                rect(20, 20, 30, 30),
                rect(5, 5, 6, 6)
            ])
        );
        assert_eq!(acc.take(), Some(vec![]));
        acc.add(&[rect(1, 1, 2, 2)]);
        assert_eq!(acc.take(), Some(vec![rect(1, 1, 2, 2)]));
    }

    #[test]
    fn damage_acc_add_unknown_poisons_until_the_next_take() {
        let mut acc = DamageAcc::new();
        acc.take();
        acc.add(&[rect(0, 0, 4, 4)]);
        acc.add_unknown();
        acc.add(&[rect(1, 1, 2, 2)]);
        assert_eq!(acc.take(), None);
        acc.add(&[rect(3, 3, 4, 4)]);
        assert_eq!(acc.take(), Some(vec![rect(3, 3, 4, 4)]));
    }

    #[test]
    fn damage_acc_empty_slice_is_unknown_and_empty_rects_change_nothing() {
        let mut acc = DamageAcc::new();
        acc.take();
        acc.add(&[rect(0, 0, 4, 4)]);
        acc.add(&[]);
        assert_eq!(acc.take(), None);
        assert_eq!(acc.take(), Some(vec![]));
        acc.add(&[rect(2, 2, 2, 9)]);
        assert_eq!(acc.take(), Some(vec![]));
    }

    #[test]
    fn damage_acc_keeps_exactly_max_rects() {
        let count = i32::try_from(MAX_RECTS).unwrap();
        let mut acc = DamageAcc::new();
        acc.take();
        let rects: Vec<PixelRect> = (0..count).map(strip).collect();
        acc.add(&rects);
        assert_eq!(acc.take(), Some(rects));
    }

    #[test]
    fn damage_acc_collapses_past_max_rects_to_one_bounding_box() {
        let count = i32::try_from(MAX_RECTS).unwrap();
        let mut acc = DamageAcc::new();
        acc.take();
        // 33 rectangles in one call.
        let rects: Vec<PixelRect> = (0..=count).map(strip).collect();
        acc.add(&rects);
        // Union of strip(0..=32): x 0..322, y 0..34.
        let bounding = rect(0, 0, 322, 34);
        acc.add(&[rect(500, 500, 510, 510)]);
        assert_eq!(acc.take(), Some(vec![bounding, rect(500, 500, 510, 510)]));

        // 33 rectangles over two calls collapse as well.
        acc.add(&rects[..16]);
        acc.add(&rects[16..]);
        assert_eq!(acc.take(), Some(vec![bounding]));
    }

    #[test]
    fn pacer_grid_at_30_fps() {
        let base = Instant::now();
        let interval = Duration::from_nanos(33_333_334);
        let mut pacer = Pacer::new(30);
        assert!(pacer.ready(base));
        assert_eq!(pacer.deadline(), None);
        pacer.delivered(base);
        assert_eq!(pacer.deadline(), Some(base + interval));
        assert!(!pacer.ready(base));
        assert!(!pacer.ready(base + interval - Duration::from_nanos(1)));
        assert!(pacer.ready(base + interval));
        assert!(pacer.ready(base + interval + Duration::from_millis(5)));
    }

    #[test]
    fn pacer_keeps_the_grid_when_a_delivery_is_late() {
        let base = Instant::now();
        let interval = Duration::from_nanos(33_333_334);
        let mut pacer = Pacer::new(30);
        pacer.delivered(base);
        pacer.delivered(base + interval + Duration::from_millis(5));
        assert_eq!(pacer.deadline(), Some(base + interval * 2));
    }

    #[test]
    fn pacer_restarts_the_slot_after_an_idle_gap() {
        let base = Instant::now();
        let interval = Duration::from_nanos(33_333_334);
        let mut pacer = Pacer::new(30);
        pacer.delivered(base);
        let later = base + interval * 10;
        assert!(pacer.ready(later));
        pacer.delivered(later);
        assert_eq!(pacer.deadline(), Some(later + interval));
    }

    #[test]
    fn pacer_delivers_30_frames_a_second_from_a_100_hz_source() {
        let base = Instant::now();
        let mut pacer = Pacer::new(30);
        let mut delivered = 0;
        for tick in 0..100 {
            let now = base + Duration::from_millis(10 * tick);
            if pacer.ready(now) {
                pacer.delivered(now);
                delivered += 1;
            }
        }
        assert_eq!(delivered, 30);
    }

    #[test]
    fn pacer_rounds_the_interval_up() {
        let base = Instant::now();
        let mut pacer = Pacer::new(7);
        pacer.delivered(base);
        assert_eq!(
            pacer.deadline(),
            Some(base + Duration::from_nanos(142_857_143))
        );
    }

    #[test]
    fn pacer_treats_zero_fps_as_one_and_huge_fps_as_one_nanosecond() {
        let base = Instant::now();
        let mut zero = Pacer::new(0);
        zero.delivered(base);
        assert_eq!(zero.deadline(), Some(base + Duration::from_secs(1)));

        let mut one = Pacer::new(1);
        one.delivered(base);
        assert_eq!(one.deadline(), Some(base + Duration::from_secs(1)));

        let mut fast = Pacer::new(u32::MAX);
        fast.delivered(base);
        assert_eq!(fast.deadline(), Some(base + Duration::from_nanos(1)));
        assert!(fast.ready(base + Duration::from_nanos(1)));
    }

    #[test]
    fn frame_time_uses_a_sane_producer_timestamp() {
        let now = MonoTime::from_nanos(10_000_000_000);
        assert_eq!(
            frame_time(Some(9_500_000_000), now),
            MonoTime::from_nanos(9_500_000_000)
        );
        // Exactly one second old is still accepted.
        assert_eq!(
            frame_time(Some(9_000_000_000), now),
            MonoTime::from_nanos(9_000_000_000)
        );
        assert_eq!(frame_time(Some(10_000_000_000), now), now);
    }

    #[test]
    fn frame_time_falls_back_to_now() {
        let now = MonoTime::from_nanos(10_000_000_000);
        assert_eq!(frame_time(None, now), now);
        assert_eq!(frame_time(Some(10_000_000_001), now), now);
        assert_eq!(frame_time(Some(0), now), now);
        assert_eq!(frame_time(Some(-5), now), now);
        assert_eq!(frame_time(Some(8_999_999_999), now), now);
        assert_eq!(frame_time(Some(i64::MAX), now), now);
        assert_eq!(frame_time(Some(i64::MIN), now), now);
    }

    #[test]
    fn mono_now_is_nonzero_and_monotonic() {
        let first = mono_now();
        let second = mono_now();
        assert!(first.as_nanos() > 0);
        assert!(second >= first);
    }
}
