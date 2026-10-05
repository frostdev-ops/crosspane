#![allow(clippy::unwrap_used)]
use crosspane_platform::{PlatformError, StreamEndReason};
use crosspane_platform_windows::model::frame_capture::{
    Latest, TargetState, crop_rect, end_reason, failure_reason, refresh_monitor, resolve_monitor,
    surface_ready, validate_crop,
};
use crosspane_platform_windows::model::{
    cursor::over_content,
    displays::{MonitorSnapshot, NativeMonitor, commit},
    geometry::{DisplayIds, MonitorProbe},
};
use crosspane_types::{
    geom::{PixelRect, PixelSize},
    id::DisplayId,
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

fn monitor(path: &str, handle: usize, bounds: [i32; 4], primary: bool) -> NativeMonitor {
    NativeMonitor {
        handle,
        probe: MonitorProbe {
            device_path: path.into(),
            name: format!("source-{path}"),
            rc_monitor: bounds,
            rc_work: bounds,
            primary,
            dpi: 96,
            refresh_millihz: 60_000,
            edid: None,
            twin: false,
            quarter_turns: 0,
        },
    }
}
fn snapshot() -> MonitorSnapshot {
    commit(
        vec![
            monitor("left", 11, [-100, -20, 0, 80], true),
            monitor("right", 22, [0, -20, 200, 80], false),
        ],
        &mut DisplayIds::default(),
    )
    .unwrap()
}
fn display(snapshot: &MonitorSnapshot, name: &str) -> DisplayId {
    snapshot
        .displays
        .iter()
        .find(|d| d.name == name)
        .unwrap()
        .id
}

#[test]
fn monitor_resolution_uses_exact_retained_id_without_primary_fallback() {
    let snapshot = snapshot();
    let left = display(&snapshot, "source-left");
    let right = display(&snapshot, "source-right");
    let before = format!("{:?}", snapshot.ids);
    let target = resolve_monitor(&snapshot, right).unwrap();
    assert_eq!(target.handle, 22);
    assert_eq!(target.device_path, "right");
    assert_eq!(target.bounds, rect(0, -20, 200, 100));
    assert_eq!(resolve_monitor(&snapshot, left).unwrap().handle, 11);
    let missing = (0..)
        .map(DisplayId)
        .find(|id| !snapshot.monitors.contains_key(id))
        .unwrap();
    assert!(matches!(
        resolve_monitor(&snapshot, missing),
        Err(PlatformError::NotFound)
    ));
    assert_eq!(format!("{:?}", snapshot.ids), before);
}

#[test]
fn removed_or_rebound_monitor_ends_old_target_but_geometry_refresh_is_allowed() {
    let first = snapshot();
    let id = display(&first, "source-right");
    let target = resolve_monitor(&first, id).unwrap();
    let mut ids = first.ids.clone();
    let moved = commit(
        vec![
            monitor("left", 11, [-100, -20, 0, 80], true),
            monitor("right", 22, [0, -20, 240, 100], false),
        ],
        &mut ids,
    )
    .unwrap();
    let refreshed = refresh_monitor(&moved, &target).unwrap();
    assert!(target.same_binding(&refreshed));
    assert_ne!(target.bounds, refreshed.bounds);
    let removed = commit(
        vec![monitor("left", 11, [-100, -20, 0, 80], true)],
        &mut ids,
    )
    .unwrap();
    assert!(matches!(
        refresh_monitor(&removed, &target),
        Err(PlatformError::NotFound)
    ));
    let replaced = commit(
        vec![
            monitor("left", 11, [-100, -20, 0, 80], true),
            monitor("right", 99, [0, -20, 240, 100], false),
        ],
        &mut ids,
    )
    .unwrap();
    assert_eq!(display(&replaced, "source-right"), id);
    assert!(matches!(
        refresh_monitor(&replaced, &target),
        Err(PlatformError::NotFound)
    ));
    let mut reused = first.clone();
    reused
        .probes
        .iter_mut()
        .find(|p| p.device_path == "right")
        .unwrap()
        .device_path = "replacement".into();
    assert!(matches!(
        refresh_monitor(&reused, &target),
        Err(PlatformError::NotFound)
    ));
}

#[test]
fn ambiguous_or_invalid_monitor_snapshot_is_failed_not_a_guessed_target() {
    let first = snapshot();
    let id = display(&first, "source-right");
    for case in 0..9 {
        let mut bad = first.clone();
        match case {
            0 => {
                bad.monitors.insert(id, 0);
            }
            1 => {
                let other = display(&bad, "source-left");
                bad.monitors.insert(other, 22);
            }
            2 => bad
                .displays
                .push(bad.displays.iter().find(|d| d.id == id).unwrap().clone()),
            3 => bad.probes.push(
                bad.probes
                    .iter()
                    .find(|p| p.name == "source-right")
                    .unwrap()
                    .clone(),
            ),
            4 => bad
                .probes
                .iter_mut()
                .find(|p| p.name == "source-right")
                .unwrap()
                .device_path
                .clear(),
            5 => {
                bad.probes
                    .iter_mut()
                    .find(|p| p.name == "source-right")
                    .unwrap()
                    .rc_monitor = [0, 0, 0, 100]
            }
            6 => {
                bad.probes
                    .iter_mut()
                    .find(|p| p.name == "source-right")
                    .unwrap()
                    .rc_monitor = [i32::MIN, 0, i32::MAX, 100]
            }
            7 => {
                bad.displays
                    .iter_mut()
                    .find(|d| d.id == id)
                    .unwrap()
                    .geometry
                    .pixel_size = PixelSize::new(1, 1)
            }
            8 => {
                bad.probes
                    .iter_mut()
                    .find(|p| p.name == "source-right")
                    .unwrap()
                    .name = "missing".into()
            }
            _ => unreachable!(),
        }
        let error = resolve_monitor(&bad, id).unwrap_err();
        assert_eq!(failure_reason(true, true, &error), StreamEndReason::Failed);
    }
}

#[test]
fn monitor_cursor_and_crop_use_physical_negative_origin_and_local_roi() {
    let snapshot = snapshot();
    let target = resolve_monitor(&snapshot, display(&snapshot, "source-left")).unwrap();
    let size = PixelSize::new(100, 100);
    let bounds = [
        target.bounds.min.x,
        target.bounds.min.y,
        target.bounds.max.x,
        target.bounds.max.y,
    ];
    let crop = Some(rect(10, 20, 30, 40));
    assert_eq!(crop_rect(size, crop), crop);
    assert!(over_content((-90, 0), bounds, size, crop, true));
    for point in [(-91, 0), (-90, -1), (-60, 0), (-90, 40), (0, 0)] {
        assert!(!over_content(point, bounds, size, crop, true));
    }
    assert!(!over_content(
        (-90, 0),
        bounds,
        PixelSize::new(120, 100),
        crop,
        true
    ));
    assert_eq!(
        crop_rect(PixelSize::new(25, 35), crop),
        Some(rect(10, 20, 15, 15))
    );
    assert_eq!(crop_rect(PixelSize::new(9, 19), crop), None);
}

#[test]
fn monitor_mapping_failure_and_gate_epoch_loss_have_distinct_end_reasons() {
    for error in [
        PlatformError::NotFound,
        PlatformError::Backend("fresh observation failed".into()),
        PlatformError::Timeout,
    ] {
        assert_eq!(
            failure_reason(false, true, &error),
            StreamEndReason::Blocked
        );
        assert_eq!(
            failure_reason(true, false, &error),
            StreamEndReason::Blocked
        );
    }
    assert_eq!(
        failure_reason(true, true, &PlatformError::NotFound),
        StreamEndReason::TargetGone
    );
    assert_eq!(
        failure_reason(true, true, &PlatformError::Timeout),
        StreamEndReason::Failed
    );
    assert_eq!(end_reason(true, true, TargetState::Live), None);
}
