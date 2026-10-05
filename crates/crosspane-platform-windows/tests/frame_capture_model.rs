#![allow(clippy::unwrap_used)]
use crosspane_platform::StreamEndReason;
use crosspane_platform_windows::model::frame_capture::{
    Latest, TargetState, crop_rect, end_reason, surface_ready, validate_crop,
};
use crosspane_types::{
    geom::{PixelRect, PixelSize},
    time::MonoTime,
};
use proptest::prelude::*;

fn time(ns: u64) -> MonoTime {
    MonoTime::from_nanos(ns)
}
fn rect(x: i32, y: i32, width: i32, height: i32) -> PixelRect {
    PixelRect::new((x, y).into(), (x + width, y + height).into())
}

#[test]
fn zero_fps_is_refused_before_native_capture() {
    assert!(Latest::<u64>::new(0).is_err());
}

#[test]
fn newest_only_delivery_obeys_exact_fps_boundary() {
    let mut queue = Latest::new(20).unwrap();
    queue.push(1);
    assert_eq!(queue.take(time(0)), Some(1));
    queue.delivered(time(0));
    queue.push(2);
    assert_eq!(queue.push(3), Some(2));
    assert_eq!(queue.take(time(49_999_999)), None);
    assert_eq!(queue.take(time(50_000_000)), Some(3));
    queue.delivered(time(50_000_000));
    queue.push(4);
    assert_eq!(queue.clear(), Some(4));
    assert_eq!(queue.take(time(100_000_000)), None);
}

#[test]
fn crop_is_target_local_clamped_after_resize_and_empty_is_skipped() {
    let wanted = rect(10, 20, 30, 40);
    assert_eq!(
        crop_rect(PixelSize::new(100, 100), Some(wanted)),
        Some(wanted)
    );
    assert_eq!(
        crop_rect(PixelSize::new(25, 35), Some(wanted)),
        Some(rect(10, 20, 15, 15))
    );
    assert_eq!(crop_rect(PixelSize::new(9, 19), Some(wanted)), None);
    assert_eq!(crop_rect(PixelSize::new(0, 10), None), None);
}

#[test]
fn invalid_crop_is_refused_before_native_capture() {
    for bad in [
        rect(-1, 0, 10, 10),
        rect(0, -1, 10, 10),
        rect(0, 0, 0, 10),
        rect(0, 0, 10, 0),
    ] {
        assert!(validate_crop(Some(bad)).is_err());
    }
    assert!(validate_crop(None).is_ok());
    assert!(validate_crop(Some(rect(1, 2, 3, 4))).is_ok());
}

#[test]
fn clipped_resize_surface_is_skipped_until_roi_is_fully_contained() {
    let grown = rect(0, 0, 586, 413);
    assert!(matches!(
        surface_ready(PixelSize::new(466, 353), grown),
        Ok(false)
    ));
    assert!(matches!(
        surface_ready(PixelSize::new(586, 413), grown),
        Ok(true)
    ));
    assert!(matches!(
        surface_ready(PixelSize::new(466, 353), rect(20, 40, 60, 60)),
        Ok(true)
    ));
    assert!(surface_ready(PixelSize::new(0, 353), grown).is_err());
    assert!(surface_ready(PixelSize::new(586, 413), rect(-1, 0, 10, 10)).is_err());
    // Skipping an incomplete frame never suppresses terminal gate or target observations.
    assert_eq!(
        end_reason(false, true, TargetState::Live),
        Some(StreamEndReason::Blocked)
    );
    assert_eq!(
        end_reason(true, true, TargetState::Gone),
        Some(StreamEndReason::TargetGone)
    );
}

#[test]
fn gate_epoch_loss_overrides_minimize_and_close_with_blocked() {
    for state in [TargetState::Live, TargetState::Minimized, TargetState::Gone] {
        assert_eq!(
            end_reason(false, true, state),
            Some(StreamEndReason::Blocked)
        );
        assert_eq!(
            end_reason(true, false, state),
            Some(StreamEndReason::Blocked)
        );
    }
    assert_eq!(end_reason(true, true, TargetState::Live), None);
}

#[test]
fn minimize_is_failed_and_confirmed_close_is_target_gone() {
    assert_eq!(
        end_reason(true, true, TargetState::Minimized),
        Some(StreamEndReason::Failed)
    );
    assert_eq!(
        end_reason(true, true, TargetState::Gone),
        Some(StreamEndReason::TargetGone)
    );
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn paced_delivery_is_newest_and_never_faster_than_max_fps(fps in 1u32..=1000, arrivals in prop::collection::vec(1u32..100_000_000,1..256)) {
        let mut queue = Latest::new(fps).unwrap();
        let interval = 1_000_000_000_u64.div_ceil(u64::from(fps));
        let mut now = 0;
        let mut last = None;
        for (sequence, step) in arrivals.into_iter().enumerate() {
            now += u64::from(step);
            queue.push(sequence);
            if let Some(newest) = queue.take(time(now)) {
                prop_assert_eq!(newest,sequence);
                if let Some(previous) = last { prop_assert!(now - previous >= interval); }
                last = Some(now);
                queue.delivered(time(now));
            }
        }
    }
}
