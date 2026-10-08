//! DRAG-v0 D-6 placed return: pure placement geometry and the `PlacedRestore` decorator over
//! fakes. Never invokes a desktop API.
#![allow(clippy::unwrap_used)]

use crosspane_platform::{Parked, ParkingKind, PlatformError, WindowParking};
use crosspane_platform_windows::model::{
    drag::{
        PlacedRestore, Placement, RESTORE_AT_BOUND, RestorePlacer, placed_origin, placement_monitor,
    },
    geometry::{DisplayIds, MonitorProbe},
};
use crosspane_types::{
    geom::{PixelRect, PixelSize, PointDevice, euclid::point2},
    id::{DisplayId, WindowId},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WINDOW: WindowId = WindowId(77);
const DISPLAY: DisplayId = DisplayId(9);

/// The 500x300 window: visible rectangle `[left, top, right, bottom]`, and its outer rectangle with
/// borders 7/0/7/7 (left, top, right, bottom).
const VISIBLE: [i32; 4] = [100, 100, 600, 400];
const OUTER: [i32; 4] = [93, 100, 607, 407];

fn probe(path: &str, rc_monitor: [i32; 4], rc_work: [i32; 4]) -> MonitorProbe {
    MonitorProbe {
        device_path: path.into(),
        name: path.into(),
        rc_monitor,
        rc_work,
        primary: true,
        dpi: 96,
        refresh_millihz: 60_000,
        edid: None,
        twin: false,
        quarter_turns: 0,
    }
}

/// 1920x1080 with a 40 px taskbar at the bottom.
fn taskbar_monitor() -> MonitorProbe {
    probe("owned-taskbar", [0, 0, 1920, 1080], [0, 0, 1920, 1040])
}

fn origin(x: f64, y: f64) -> PointDevice {
    PointDevice::new(x, y)
}

fn assert_backend<T: std::fmt::Debug>(result: Result<T, PlatformError>, text: &str) {
    match result {
        Err(PlatformError::Backend(message)) => assert_eq!(message, text),
        other => panic!("expected Backend({text:?}), got {other:?}"),
    }
}

fn assert_not_found<T: std::fmt::Debug>(result: Result<T, PlatformError>) {
    match result {
        Err(PlatformError::NotFound) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn inside_the_work_area_lands_exactly_and_keeps_the_outer_offset() {
    assert_eq!(
        placed_origin(OUTER, VISIBLE, origin(37.0, 41.0), &taskbar_monitor()).unwrap(),
        Placement {
            outer: (30, 41),
            visible: (37, 41),
        }
    );
    // The requested origin is relative to the monitor, so a monitor at x = 1920 shifts it.
    let right = probe("owned-right", [1920, 0, 3840, 1080], [1920, 0, 3840, 1040]);
    assert_eq!(
        placed_origin(OUTER, VISIBLE, origin(37.0, 41.0), &right).unwrap(),
        Placement {
            outer: (1950, 41),
            visible: (1957, 41),
        }
    );
}

#[test]
fn right_and_bottom_clamp_to_the_taskbar_work_area() {
    // The 500x300 window fits between x 0..=1420 and y 0..=740 of the 1920x1040 work area.
    let monitor = taskbar_monitor();
    assert_eq!(
        placed_origin(OUTER, VISIBLE, origin(1_000_000.0, 1_000_000.0), &monitor).unwrap(),
        Placement {
            outer: (1413, 740),
            visible: (1420, 740),
        }
    );
    assert_eq!(
        placed_origin(OUTER, VISIBLE, origin(1500.0, 10.0), &monitor).unwrap(),
        Placement {
            outer: (1413, 10),
            visible: (1420, 10),
        }
    );
    assert_eq!(
        placed_origin(OUTER, VISIBLE, origin(10.0, 900.0), &monitor).unwrap(),
        Placement {
            outer: (3, 740),
            visible: (10, 740),
        }
    );
}

#[test]
fn negative_origin_clamps_to_the_work_area_top_left() {
    // A taskbar on the left and top: the work area starts at (40, 40).
    let monitor = probe("owned-left-top", [0, 0, 1920, 1080], [40, 40, 1920, 1040]);
    assert_eq!(
        placed_origin(OUTER, VISIBLE, origin(-50.0, -20.0), &monitor).unwrap(),
        Placement {
            outer: (33, 40),
            visible: (40, 40),
        }
    );
}

#[test]
fn oversized_window_anchors_at_the_work_area_top_left() {
    // 2000x1200 does not fit the 1920x1040 work area, so both axes anchor at the top-left.
    let visible = [100, 100, 2100, 1300];
    let outer = [93, 100, 2107, 1307];
    for target in [origin(500.0, 500.0), origin(-9000.0, 9000.0)] {
        assert_eq!(
            placed_origin(outer, visible, target, &taskbar_monitor()).unwrap(),
            Placement {
                outer: (-7, 0),
                visible: (0, 0),
            }
        );
    }
}

#[test]
fn negative_origin_monitor_places_in_its_own_coordinates() {
    let monitor = probe(
        "owned-left-monitor",
        [-1920, 0, 0, 1080],
        [-1920, 0, 0, 1040],
    );
    let outer = [-1807, 100, -1293, 407];
    let visible = [-1800, 100, -1300, 400];
    assert_eq!(
        placed_origin(outer, visible, origin(10.0, 20.0), &monitor).unwrap(),
        Placement {
            outer: (-1917, 20),
            visible: (-1910, 20),
        }
    );
    assert_eq!(
        placed_origin(outer, visible, origin(-5000.0, -5000.0), &monitor).unwrap(),
        Placement {
            outer: (-1927, 0),
            visible: (-1920, 0),
        }
    );
    assert_eq!(
        placed_origin(outer, visible, origin(1_000_000.0, 1_000_000.0), &monitor).unwrap(),
        Placement {
            outer: (-507, 740),
            visible: (-500, 740),
        }
    );
}

#[test]
fn non_finite_origin_is_an_invalid_placement() {
    for target in [
        origin(f64::NAN, 0.0),
        origin(0.0, f64::INFINITY),
        origin(f64::NEG_INFINITY, 0.0),
    ] {
        assert_backend(
            placed_origin(OUTER, VISIBLE, target, &taskbar_monitor()),
            "invalid placement geometry",
        );
    }
}

#[test]
fn inverted_rectangles_are_an_invalid_placement() {
    let target = origin(37.0, 41.0);
    let monitor = taskbar_monitor();
    assert_backend(
        placed_origin([107, 100, 93, 407], VISIBLE, target, &monitor),
        "invalid placement geometry",
    );
    assert_backend(
        placed_origin(OUTER, [600, 100, 100, 400], target, &monitor),
        "invalid placement geometry",
    );
    assert_backend(
        placed_origin(OUTER, [100, 400, 600, 100], target, &monitor),
        "invalid placement geometry",
    );
}

#[test]
fn placement_arithmetic_that_overflows_i32_is_an_invalid_placement() {
    // The monitor starts at x = -2e9. The outer left edge sits 200 000 007 px further left than the
    // visible one, so the outer result leaves i32 range. The 7 px control stays representable.
    let far_left = probe(
        "owned-far-left",
        [-2_000_000_000, 0, -1_999_998_080, 1080],
        [-2_000_000_000, 0, -1_999_998_080, 1040],
    );
    let visible = [0, 100, 500, 400];
    assert_backend(
        placed_origin(
            [-200_000_007, 100, 507, 407],
            visible,
            origin(0.0, 0.0),
            &far_left,
        ),
        "invalid placement geometry",
    );
    assert_eq!(
        placed_origin([-7, 100, 507, 407], visible, origin(0.0, 0.0), &far_left).unwrap(),
        Placement {
            outer: (-2_000_000_007, 0),
            visible: (-2_000_000_000, 0),
        }
    );
    // The same overflow on the vertical axis.
    let far_top = probe(
        "owned-far-top",
        [0, -2_000_000_000, 1920, -1_999_998_920],
        [0, -2_000_000_000, 1920, -1_999_998_960],
    );
    assert_backend(
        placed_origin(
            [100, -200_000_007, 407, 307],
            [100, 0, 400, 300],
            origin(0.0, 0.0),
            &far_top,
        ),
        "invalid placement geometry",
    );
}

#[test]
fn placement_monitor_returns_the_unique_non_twin_probe_for_the_display() {
    let primary = probe("owned-a", [0, 0, 1920, 1080], [0, 0, 1920, 1040]);
    let mut secondary = probe("owned-b", [1920, 0, 3840, 1080], [1920, 0, 3840, 1040]);
    secondary.primary = false;
    let mut twin = probe("owned-twin", [4000, 0, 5920, 1080], [4000, 0, 5920, 1080]);
    twin.primary = false;
    twin.twin = true;
    let probes = [primary.clone(), secondary.clone(), twin];
    let mut ids = DisplayIds::default();
    let first = ids.assign("owned-a").unwrap();
    let second = ids.assign("owned-b").unwrap();
    assert_eq!(
        placement_monitor(first, &probes, &mut ids).unwrap(),
        primary
    );
    assert_eq!(
        placement_monitor(second, &probes, &mut ids).unwrap(),
        secondary
    );
}

#[test]
fn placement_monitor_for_an_unknown_display_is_not_found() {
    let probes = [taskbar_monitor()];
    let mut ids = DisplayIds::default();
    let ghost = ids.assign("owned-ghost").unwrap();
    assert_not_found(placement_monitor(ghost, &probes, &mut ids));
}

#[test]
fn placement_monitor_for_a_twin_only_display_is_not_found() {
    let mut twin = probe("owned-twin", [0, 0, 1920, 1080], [0, 0, 1920, 1080]);
    twin.primary = false;
    twin.twin = true;
    let probes = [twin];
    let mut ids = DisplayIds::default();
    let display = ids.assign("owned-twin").unwrap();
    assert_not_found(placement_monitor(display, &probes, &mut ids));
}

#[test]
fn duplicate_device_paths_fail_the_placement_display_layout() {
    let first = taskbar_monitor();
    let mut second = probe(
        "owned-taskbar",
        [1920, 0, 3840, 1080],
        [1920, 0, 3840, 1040],
    );
    second.primary = false;
    let probes = [first, second];
    let mut ids = DisplayIds::default();
    let display = ids.assign("owned-taskbar").unwrap();
    assert_backend(
        placement_monitor(display, &probes, &mut ids),
        "placement display layout",
    );
}

/// Recorded calls, in order, across the parking fake and the placer fake.
#[derive(Clone, Debug, PartialEq)]
enum Call {
    Park(WindowId),
    Resize(WindowId),
    SetFullscreen(WindowId, bool),
    Geometry(WindowId),
    Restore(WindowId),
    Recover,
    Place(WindowId, DisplayId, (f64, f64)),
}

type Log = Arc<Mutex<Vec<Call>>>;

fn parked(window: WindowId) -> Parked {
    Parked {
        window,
        kind: ParkingKind::Mirror,
        display: DISPLAY,
        content: PixelRect::new(point2(10, 20), point2(510, 320)),
        fullscreen: false,
    }
}

#[derive(Debug)]
struct FakeParking {
    log: Log,
    fail_restore: bool,
}

impl WindowParking for FakeParking {
    fn park(
        &mut self,
        window: WindowId,
        _size: PixelSize,
        _scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.log.lock().unwrap().push(Call::Park(window));
        Ok(parked(window))
    }

    fn resize(
        &mut self,
        window: WindowId,
        _size: PixelSize,
        _scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.log.lock().unwrap().push(Call::Resize(window));
        Ok(parked(window))
    }

    fn set_fullscreen(&mut self, window: WindowId, fullscreen: bool) -> Result<(), PlatformError> {
        self.log
            .lock()
            .unwrap()
            .push(Call::SetFullscreen(window, fullscreen));
        Ok(())
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        self.log.lock().unwrap().push(Call::Geometry(window));
        Ok(parked(window))
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        self.log.lock().unwrap().push(Call::Restore(window));
        if self.fail_restore {
            return Err(PlatformError::Backend("restore refused".into()));
        }
        Ok(())
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        self.log.lock().unwrap().push(Call::Recover);
        Ok(vec![WindowId(5)])
    }
}

#[derive(Debug)]
struct FakePlacer {
    log: Log,
    fail: bool,
    deadline: Arc<Mutex<Option<Instant>>>,
}

impl RestorePlacer for FakePlacer {
    fn place(
        &mut self,
        window: WindowId,
        display: DisplayId,
        origin: PointDevice,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        self.log
            .lock()
            .unwrap()
            .push(Call::Place(window, display, (origin.x, origin.y)));
        *self.deadline.lock().unwrap() = Some(deadline);
        if self.fail {
            return Err(PlatformError::Backend("placement refused".into()));
        }
        Ok(())
    }
}

struct Harness {
    log: Log,
    deadline: Arc<Mutex<Option<Instant>>>,
    reports: Arc<Mutex<Vec<String>>>,
    wrapped: PlacedRestore<FakeParking, FakePlacer>,
}

fn harness(fail_restore: bool, fail_place: bool) -> Harness {
    let log: Log = Arc::default();
    let deadline: Arc<Mutex<Option<Instant>>> = Arc::default();
    let reports: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&reports);
    let wrapped = PlacedRestore::new(
        FakeParking {
            log: Arc::clone(&log),
            fail_restore,
        },
        FakePlacer {
            log: Arc::clone(&log),
            fail: fail_place,
            deadline: Arc::clone(&deadline),
        },
        Box::new(move |error: &PlatformError| sink.lock().unwrap().push(error.to_string())),
    );
    Harness {
        log,
        deadline,
        reports,
        wrapped,
    }
}

fn calls(harness: &Harness) -> Vec<Call> {
    harness.log.lock().unwrap().clone()
}

#[test]
fn restore_and_place_both_ok_returns_ok_in_restore_then_place_order() {
    let mut h = harness(false, false);
    h.wrapped
        .restore_at(WINDOW, DISPLAY, origin(37.0, 41.0))
        .unwrap();
    assert_eq!(
        calls(&h),
        vec![
            Call::Restore(WINDOW),
            Call::Place(WINDOW, DISPLAY, (37.0, 41.0)),
        ]
    );
    assert!(h.reports.lock().unwrap().is_empty());
}

#[test]
fn restore_error_propagates_and_never_places() {
    let mut h = harness(true, false);
    assert_backend(
        h.wrapped.restore_at(WINDOW, DISPLAY, origin(37.0, 41.0)),
        "restore refused",
    );
    assert_eq!(calls(&h), vec![Call::Restore(WINDOW)]);
    assert!(h.reports.lock().unwrap().is_empty());
}

#[test]
fn placement_error_after_a_restore_is_ok_and_reported_once() {
    let mut h = harness(false, true);
    h.wrapped
        .restore_at(WINDOW, DISPLAY, origin(37.0, 41.0))
        .unwrap();
    assert_eq!(
        calls(&h),
        vec![
            Call::Restore(WINDOW),
            Call::Place(WINDOW, DISPLAY, (37.0, 41.0)),
        ]
    );
    assert_eq!(
        *h.reports.lock().unwrap(),
        vec!["placement refused".to_string()]
    );
}

#[test]
fn placement_deadline_is_at_most_the_restore_at_bound_ahead() {
    assert_eq!(RESTORE_AT_BOUND, Duration::from_secs(2));
    let mut h = harness(false, false);
    let before = Instant::now();
    h.wrapped
        .restore_at(WINDOW, DISPLAY, origin(37.0, 41.0))
        .unwrap();
    let after = Instant::now();
    let deadline = h.deadline.lock().unwrap().unwrap();
    assert!(deadline >= before + RESTORE_AT_BOUND);
    assert!(deadline <= after + RESTORE_AT_BOUND);
}

#[test]
fn restore_never_places_even_when_the_placer_would_fail() {
    let mut h = harness(false, true);
    h.wrapped.restore(WINDOW).unwrap();
    assert_eq!(calls(&h), vec![Call::Restore(WINDOW)]);
    assert!(h.reports.lock().unwrap().is_empty());
}

#[test]
fn park_resize_set_fullscreen_geometry_and_recover_delegate_unchanged() {
    let mut h = harness(false, true);
    let size = PixelSize::new(500, 360);
    assert_eq!(h.wrapped.park(WINDOW, size, 1.5).unwrap(), parked(WINDOW));
    assert_eq!(h.wrapped.resize(WINDOW, size, 2.0).unwrap(), parked(WINDOW));
    h.wrapped.set_fullscreen(WINDOW, true).unwrap();
    assert_eq!(h.wrapped.geometry(WINDOW).unwrap(), parked(WINDOW));
    assert_eq!(h.wrapped.recover().unwrap(), vec![WindowId(5)]);
    assert_eq!(
        calls(&h),
        vec![
            Call::Park(WINDOW),
            Call::Resize(WINDOW),
            Call::SetFullscreen(WINDOW, true),
            Call::Geometry(WINDOW),
            Call::Recover,
        ]
    );
    assert!(h.reports.lock().unwrap().is_empty());
}
