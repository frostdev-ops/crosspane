#![allow(clippy::unwrap_used)]

use crosspane_platform::{CaptureEvent, CapturePortal, Edge, PlatformError, PortalId};
use crosspane_platform_windows::model::drag::{Detector, WindowFact};
use crosspane_platform_windows::model::window::Identity;
use crosspane_types::{
    geom::{PixelRect, PointDevice, euclid::point2},
    id::{DisplayId, WindowId},
    time::MonoTime,
};

fn at(ms: u64) -> MonoTime {
    MonoTime::from_nanos(ms * 1_000_000)
}

fn fact(pointer: (i32, i32)) -> WindowFact {
    WindowFact {
        window: WindowId(77),
        identity: Identity {
            hwnd: 100,
            pid: 200,
            tid: 300,
            process_created: 400,
        },
        content: PixelRect::new(
            point2(pointer.0 - 50, pointer.1 + 20),
            point2(pointer.0 + 150, pointer.1 + 170),
        ),
    }
}

fn portal(edge: Edge) -> (CapturePortal, PixelRect) {
    let rect = match edge {
        Edge::Left => PixelRect::new(point2(0, 100), point2(1, 400)),
        Edge::Right => PixelRect::new(point2(999, 100), point2(1000, 400)),
        Edge::Top => PixelRect::new(point2(100, 0), point2(400, 1)),
        Edge::Bottom => PixelRect::new(point2(100, 599), point2(400, 600)),
    };
    (
        CapturePortal {
            id: PortalId(1),
            display: DisplayId(9),
            edge,
            from: 100.0,
            to: 400.0,
        },
        rect,
    )
}

fn detector(edge: Edge) -> Detector {
    let mut detector = Detector::default();
    assert!(
        detector
            .set_portals(&[portal(edge)], at(0))
            .unwrap()
            .is_empty()
    );
    detector
}

fn crossed(edge: Edge) -> (Detector, (i32, i32), (f64, f64), Vec<CaptureEvent>) {
    let (before, point, delta) = match edge {
        Edge::Left => ((9, 200), (0, 200), (-9.0, 0.0)),
        Edge::Right => ((990, 200), (999, 200), (9.0, 0.0)),
        Edge::Top => ((200, 9), (200, 0), (0.0, -9.0)),
        Edge::Bottom => ((200, 590), (200, 599), (0.0, 9.0)),
    };
    let mut detector = detector(edge);
    assert!(
        detector
            .start(fact(before), before, true, at(10))
            .is_empty()
    );
    let events = detector.sample(Some(fact(point)), point, Some(delta), true, at(20));
    (detector, point, delta, events)
}

#[test]
fn native_move_plus_stable_content_and_outward_primary_detects_all_four_edges() {
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let (_, _, _, events) = crossed(edge);
        assert_eq!(
            events,
            vec![CaptureEvent::DragAtEdge {
                portal: PortalId(1),
                position: 1.0 / 3.0,
                window: WindowId(77),
                grab: PointDevice::new(50.0, -20.0),
                at: at(20),
            }]
        );
    }
}

#[test]
fn no_start_keyboard_move_or_non_primary_only_never_classifies() {
    for started in [false, true] {
        let mut detector = detector(Edge::Right);
        if started {
            detector.start(fact((990, 200)), (990, 200), false, at(10));
        }
        assert!(
            detector
                .sample(
                    Some(fact((999, 200))),
                    (999, 200),
                    Some((9.0, 0.0)),
                    true,
                    at(20)
                )
                .is_empty()
        );
        assert_eq!(detector.at_edge(PortalId(1)), None);
    }
}

#[test]
fn resize_including_left_origin_motion_is_not_a_window_move() {
    let mut detector = detector(Edge::Left);
    detector.start(fact((9, 200)), (9, 200), true, at(10));
    let mut resized = fact((0, 200));
    resized.content.max.x += 9;
    assert!(
        detector
            .sample(Some(resized), (0, 200), Some((-9.0, 0.0)), true, at(20))
            .is_empty()
    );
    assert!(
        detector
            .sample(Some(resized), (0, 200), Some((-2.0, 0.0)), true, at(40))
            .is_empty()
    );
}

#[test]
fn unchanged_window_with_cursor_motion_is_not_a_native_move() {
    let mut detector = detector(Edge::Right);
    let still = fact((990, 200));
    detector.start(still, (990, 200), true, at(10));
    assert!(
        detector
            .sample(Some(still), (999, 200), Some((9.0, 0.0)), true, at(20))
            .is_empty()
    );
}

#[test]
fn every_native_identity_field_and_admitted_window_generation_is_bound() {
    for field in 0..5 {
        let (mut detector, point, delta, _) = crossed(Edge::Right);
        let mut changed = fact(point);
        match field {
            0 => changed.window = WindowId(78),
            1 => changed.identity.hwnd += 1,
            2 => changed.identity.pid += 1,
            3 => changed.identity.tid += 1,
            4 => changed.identity.process_created += 1,
            _ => unreachable!(),
        }
        assert_eq!(
            detector.sample(Some(changed), point, Some(delta), true, at(40)),
            vec![CaptureEvent::EdgeReleased {
                portal: PortalId(1),
                at: at(40)
            }]
        );
        assert_eq!(detector.at_edge(PortalId(1)), None);
    }
}

#[test]
fn outward_motion_is_required_to_enter_and_clamped_outward_delta_is_accepted() {
    for delta in [(0.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (f64::NAN, 0.0)] {
        let mut detector = detector(Edge::Right);
        detector.start(fact((990, 200)), (990, 200), true, at(10));
        assert!(
            detector
                .sample(
                    Some(fact((999, 200))),
                    (999, 200),
                    Some(delta),
                    true,
                    at(20)
                )
                .is_empty()
        );
    }
    let (mut detector, point, _, _) = crossed(Edge::Right);
    assert!(matches!(
        detector.sample(Some(fact(point)), point, Some((1.0, 0.0)), true, at(40)).as_slice(),
        [CaptureEvent::DragAtEdge { at: observed, .. }] if *observed == at(40)
    ));
}

#[test]
fn held_edge_repeats_boundedly_and_inward_motion_releases_it() {
    let (mut detector, point, _, _) = crossed(Edge::Right);
    assert!(
        detector
            .sample(Some(fact(point)), point, Some((0.0, 0.0)), true, at(25))
            .is_empty()
    );
    assert!(matches!(
        detector
            .sample(Some(fact(point)), point, Some((0.0, 0.0)), true, at(40))
            .as_slice(),
        [CaptureEvent::DragAtEdge { .. }]
    ));
    assert_eq!(
        detector.sample(Some(fact(point)), point, Some((-1.0, 0.0)), true, at(60)),
        vec![CaptureEvent::EdgeReleased {
            portal: PortalId(1),
            at: at(60)
        }]
    );
}

#[test]
fn end_primary_release_other_button_and_unavailable_geometry_clear_edge() {
    for reason in 0..4 {
        let (mut detector, point, delta, _) = crossed(Edge::Right);
        let events = match reason {
            0 => detector.end(at(40)),
            1 | 2 => detector.sample(Some(fact(point)), point, Some(delta), false, at(40)),
            3 => detector.sample(None, point, Some(delta), true, at(40)),
            _ => unreachable!(),
        };
        assert_eq!(
            events,
            vec![CaptureEvent::EdgeReleased {
                portal: PortalId(1),
                at: at(40)
            }]
        );
        assert_eq!(detector.at_edge(PortalId(1)), None);
        assert!(detector.end(at(50)).is_empty());
    }
}

#[test]
fn portal_removal_releases_and_invalid_replacement_preserves_old_portal() {
    let (mut detector, _, _, _) = crossed(Edge::Right);
    let mut invalid = portal(Edge::Right);
    invalid.0.to = invalid.0.from;
    assert!(matches!(
        detector.set_portals(&[invalid], at(30)),
        Err(PlatformError::Backend(_))
    ));
    assert!(detector.at_edge(PortalId(1)).is_some());
    assert_eq!(
        detector.set_portals(&[], at(40)).unwrap(),
        vec![CaptureEvent::EdgeReleased {
            portal: PortalId(1),
            at: at(40)
        }]
    );
}

#[test]
fn negative_virtual_desktop_coordinates_keep_content_grab_without_clamping() {
    let mut p = portal(Edge::Left);
    p.1 = PixelRect::new(point2(-1000, -400), point2(-999, -100));
    let mut detector = Detector::default();
    detector.set_portals(&[p], at(0)).unwrap();
    detector.start(fact((-991, -300)), (-991, -300), true, at(10));
    let events = detector.sample(
        Some(fact((-1000, -300))),
        (-1000, -300),
        Some((-9.0, 0.0)),
        true,
        at(20),
    );
    assert!(matches!(
        events.as_slice(),
        [CaptureEvent::DragAtEdge { grab, position, .. }]
            if *grab == PointDevice::new(50.0, -20.0) && *position == 1.0 / 3.0
    ));
}
