use core::time::Duration;

use crosspane_media::hybrid::{FramePlan, HybridConfig, HybridScheduler};
use proptest::prelude::*;

fn plan(scheduler: &mut HybridScheduler, changed: u32, now_ms: u64, available: bool) -> FramePlan {
    scheduler.plan(changed, 100, Duration::from_millis(now_ms), available)
}

fn enter_video(scheduler: &mut HybridScheduler) {
    assert_eq!(plan(scheduler, 25, 0, true), FramePlan::Tiles);
    assert_eq!(plan(scheduler, 25, 1, true), FramePlan::Tiles);
    assert_eq!(plan(scheduler, 25, 2, true), FramePlan::Video { key: true });
    assert!(scheduler.in_video());
}

#[test]
fn starts_in_tiles_and_enters_at_inclusive_threshold() {
    let mut scheduler = HybridScheduler::new(HybridConfig::default());
    assert!(!scheduler.in_video());
    for now in 0..5 {
        assert_eq!(plan(&mut scheduler, 24, now, true), FramePlan::Tiles);
    }
    assert_eq!(plan(&mut scheduler, 25, 5, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 25, 6, true), FramePlan::Tiles);
    assert_eq!(
        plan(&mut scheduler, 25, 7, true),
        FramePlan::Video { key: true }
    );
    assert!(scheduler.in_video());
    assert_eq!(
        plan(&mut scheduler, 25, 8, true),
        FramePlan::Video { key: false }
    );
}

#[test]
fn single_high_change_frame_does_not_enter_and_low_change_resets_counter() {
    let mut scheduler = HybridScheduler::new(HybridConfig::default());
    assert_eq!(plan(&mut scheduler, 100, 0, true), FramePlan::Tiles);
    assert!(!scheduler.in_video());
    assert_eq!(plan(&mut scheduler, 0, 1, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 2, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 3, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 24, 4, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 5, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 6, true), FramePlan::Tiles);
    assert_eq!(
        plan(&mut scheduler, 100, 7, true),
        FramePlan::Video { key: true }
    );
}

#[test]
fn leaves_at_inclusive_exit_fraction_and_duration() {
    let mut scheduler = HybridScheduler::new(HybridConfig::default());
    enter_video(&mut scheduler);
    for now in [10, 409] {
        assert_eq!(
            plan(&mut scheduler, 2, now, true),
            FramePlan::Video { key: false }
        );
        assert!(scheduler.in_video());
    }
    assert_eq!(plan(&mut scheduler, 2, 410, true), FramePlan::TilesKey);
    assert!(!scheduler.in_video());
    assert_eq!(plan(&mut scheduler, 2, 411, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 25, 412, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 25, 413, true), FramePlan::Tiles);
    assert_eq!(
        plan(&mut scheduler, 25, 414, true),
        FramePlan::Video { key: true }
    );
}

#[test]
fn brief_stillness_does_not_leave_and_above_exit_threshold_resets_timer() {
    let mut scheduler = HybridScheduler::new(HybridConfig::default());
    enter_video(&mut scheduler);
    for (changed, now) in [(0, 10), (0, 409), (3, 410), (0, 411), (0, 810)] {
        assert_eq!(
            plan(&mut scheduler, changed, now, true),
            FramePlan::Video { key: false }
        );
    }
    assert_eq!(plan(&mut scheduler, 0, 811, true), FramePlan::TilesKey);
}

#[test]
fn unavailable_video_resets_entry_and_leaves_video_with_one_tile_key() {
    let mut scheduler = HybridScheduler::new(HybridConfig::default());
    for now in 0..5 {
        assert_eq!(plan(&mut scheduler, 100, now, false), FramePlan::Tiles);
        assert!(!scheduler.in_video());
    }
    assert_eq!(plan(&mut scheduler, 100, 5, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 6, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 7, false), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 8, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 9, true), FramePlan::Tiles);
    assert_eq!(
        plan(&mut scheduler, 100, 10, true),
        FramePlan::Video { key: true }
    );
    assert_eq!(plan(&mut scheduler, 100, 11, false), FramePlan::TilesKey);
    assert!(!scheduler.in_video());
    assert_eq!(plan(&mut scheduler, 100, 12, false), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 13, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 14, true), FramePlan::Tiles);
    assert_eq!(
        plan(&mut scheduler, 100, 15, true),
        FramePlan::Video { key: true }
    );
}

#[test]
fn video_failure_sends_one_tile_key_and_retries_at_deadline_with_fresh_counter() {
    let mut scheduler = HybridScheduler::new(HybridConfig {
        retry_after: Duration::from_secs(1),
        ..HybridConfig::default()
    });
    enter_video(&mut scheduler);
    scheduler.video_failed(Duration::from_millis(10));
    assert!(!scheduler.in_video());
    assert_eq!(plan(&mut scheduler, 100, 10, false), FramePlan::TilesKey);
    for now in [11, 500, 1009] {
        assert_eq!(plan(&mut scheduler, 100, now, true), FramePlan::Tiles);
    }
    assert_eq!(plan(&mut scheduler, 100, 1010, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 1011, true), FramePlan::Tiles);
    assert_eq!(
        plan(&mut scheduler, 100, 1012, true),
        FramePlan::Video { key: true }
    );
    assert_eq!(
        plan(&mut scheduler, 100, 1013, true),
        FramePlan::Video { key: false }
    );
}

#[test]
fn failure_in_tiles_resets_entry_without_requesting_a_tile_key() {
    let mut scheduler = HybridScheduler::new(HybridConfig {
        retry_after: Duration::from_secs(1),
        ..HybridConfig::default()
    });
    assert_eq!(plan(&mut scheduler, 100, 0, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 1, true), FramePlan::Tiles);
    scheduler.video_failed(Duration::from_millis(2));
    assert_eq!(plan(&mut scheduler, 100, 2, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 1001, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 1002, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 1003, true), FramePlan::Tiles);
    assert_eq!(
        plan(&mut scheduler, 100, 1004, true),
        FramePlan::Video { key: true }
    );
}

#[test]
fn backwards_time_does_not_advance_stillness() {
    let mut scheduler = HybridScheduler::new(HybridConfig::default());
    enter_video(&mut scheduler);
    for now in [100, 50, 99, 499] {
        assert_eq!(
            plan(&mut scheduler, 0, now, true),
            FramePlan::Video { key: false }
        );
    }
    assert_eq!(plan(&mut scheduler, 0, 500, true), FramePlan::TilesKey);
}

#[test]
fn backwards_failure_time_does_not_shorten_retry() {
    let mut scheduler = HybridScheduler::new(HybridConfig {
        retry_after: Duration::from_secs(1),
        ..HybridConfig::default()
    });
    enter_video(&mut scheduler);
    assert_eq!(
        plan(&mut scheduler, 100, 100, true),
        FramePlan::Video { key: false }
    );
    scheduler.video_failed(Duration::from_millis(50));
    assert_eq!(plan(&mut scheduler, 100, 50, true), FramePlan::TilesKey);
    for now in [0, 99, 1099, 1100, 1101] {
        assert_eq!(plan(&mut scheduler, 100, now, true), FramePlan::Tiles);
    }
    assert_eq!(
        plan(&mut scheduler, 100, 1102, true),
        FramePlan::Video { key: true }
    );
}

#[test]
fn empty_captures_return_tiles_reset_counters_and_preserve_pending_key() {
    let mut scheduler = HybridScheduler::new(HybridConfig::default());
    assert_eq!(plan(&mut scheduler, 100, 0, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 1, true), FramePlan::Tiles);
    assert_eq!(
        scheduler.plan(100, 0, Duration::from_millis(2), true),
        FramePlan::Tiles
    );
    assert_eq!(plan(&mut scheduler, 100, 3, true), FramePlan::Tiles);
    assert_eq!(plan(&mut scheduler, 100, 4, true), FramePlan::Tiles);
    assert_eq!(
        plan(&mut scheduler, 100, 5, true),
        FramePlan::Video { key: true }
    );
    assert_eq!(
        scheduler.plan(0, 0, Duration::from_millis(6), false),
        FramePlan::Tiles
    );
    assert!(scheduler.in_video());
    assert_eq!(plan(&mut scheduler, 0, 7, false), FramePlan::TilesKey);
    assert!(!scheduler.in_video());

    let mut scheduler = HybridScheduler::new(HybridConfig::default());
    enter_video(&mut scheduler);
    scheduler.video_failed(Duration::from_millis(10));
    for now in [10, 11] {
        assert_eq!(
            scheduler.plan(0, 0, Duration::from_millis(now), true),
            FramePlan::Tiles
        );
    }
    assert_eq!(plan(&mut scheduler, 100, 12, true), FramePlan::TilesKey);
    assert_eq!(plan(&mut scheduler, 100, 13, true), FramePlan::Tiles);
}

proptest! {
    #[test]
    fn tile_key_separates_video_and_tile_runs(
        events in prop::collection::vec(
            (0_u32..=100, any::<bool>(), any::<bool>(), 0_u16..=1500, any::<bool>(), any::<bool>()),
            1..256,
        ),
    ) {
        let mut scheduler = HybridScheduler::new(HybridConfig {
            retry_after: Duration::from_millis(500),
            ..HybridConfig::default()
        });
        // Every generated sequence starts with a video run, so the property is never vacuous.
        enter_video(&mut scheduler);
        let mut now = Duration::from_millis(2);
        let mut video_since_key = true;
        for (changed, available, failed, elapsed_ms, backwards, empty) in events {
            let elapsed = Duration::from_millis(u64::from(elapsed_ms));
            now = if backwards { now.saturating_sub(elapsed) } else { now.saturating_add(elapsed) };
            if failed {
                scheduler.video_failed(now);
            }
            let total = if empty { 0 } else { 100 };
            let result = scheduler.plan(changed, total, now, available);
            if total == 0 {
                // Empty captures explicitly return Tiles; they send no frame or mode transition.
                prop_assert_eq!(result, FramePlan::Tiles);
                continue;
            }
            match result {
                FramePlan::Video { .. } => {
                    prop_assert!(scheduler.in_video());
                    video_since_key = true;
                }
                FramePlan::TilesKey => {
                    prop_assert!(!scheduler.in_video());
                    video_since_key = false;
                }
                FramePlan::Tiles => {
                    prop_assert!(!scheduler.in_video());
                    prop_assert!(!video_since_key, "video must be followed by a tile key before deltas");
                }
            }
        }
    }
}
