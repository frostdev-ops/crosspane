use super::*;

#[test]
fn tiling_size_guard_waits_through_stale_quartz_before_position() {
    let before = RectLogical::new(
        PointLogical::new(100.0, 100.0),
        SizeLogical::new(400.0, 300.0),
    );
    let tile = RectLogical::new(
        PointLogical::new(900.0, 0.0),
        SizeLogical::new(900.0, 1100.0),
    );
    let window = WindowFact {
        window: WindowId(42),
        pid: 7,
        frame: tile,
        scale: 1.0,
    };
    let mut replies = std::collections::VecDeque::from([
        (WindowId(42), 7, tile, true),
        (
            WindowId(42),
            7,
            RectLogical::new(tile.origin, before.size),
            true,
        ),
    ]);
    let pauses = std::cell::Cell::new(0);
    assert!(
        restored_size_ready(
            window,
            before,
            tile,
            Instant::now() + Duration::from_millis(500),
            || Ok(replies.pop_front().unwrap()),
            || {
                pauses.set(pauses.get() + 1);
                Ok(())
            }
        )
        .unwrap()
    );
    assert_eq!(pauses.get(), 1);
    assert!(replies.is_empty());
}

#[test]
fn tiling_size_guard_keeps_deadline_and_refuses_identity_or_external_geometry_change() {
    let before = RectLogical::new(
        PointLogical::new(100.0, 100.0),
        SizeLogical::new(400.0, 300.0),
    );
    let tile = RectLogical::new(
        PointLogical::new(900.0, 0.0),
        SizeLogical::new(900.0, 1100.0),
    );
    let window = WindowFact {
        window: WindowId(42),
        pid: 7,
        frame: tile,
        scale: 1.0,
    };
    for reply in [
        (WindowId(43), 7, tile, true),
        (WindowId(42), 8, tile, true),
        (WindowId(42), 7, tile, false),
    ] {
        let deadline = Instant::now() + Duration::from_millis(500);
        assert!(
            !restored_size_ready(
                window,
                before,
                tile,
                deadline,
                || Ok(reply),
                || panic!("must not wait through an identity/visibility change")
            )
            .unwrap()
        );
    }
    let changed = RectLogical::new(
        PointLogical::new(500.0, 0.0),
        SizeLogical::new(700.0, 900.0),
    );
    assert!(
        !restored_size_ready(
            window,
            before,
            tile,
            Instant::now() + Duration::from_millis(500),
            || Ok((WindowId(42), 7, changed, true)),
            || panic!("must not wait through another resize")
        )
        .unwrap()
    );
    assert!(matches!(
        restored_size_ready(
            window,
            before,
            tile,
            Instant::now(),
            || panic!("expired deadline must not read"),
            || panic!("expired deadline must not pause")
        ),
        Err(PlatformError::Timeout)
    ));
}

#[test]
fn partly_offscreen_window_uses_visible_pointer_display_and_preserves_source_scale() {
    let frame = RectLogical::new(
        PointLogical::new(1719.0, 100.0),
        SizeLogical::new(400.0, 300.0),
    );
    let pointer = PointLogical::new(1799.0, 112.0);
    let scale = scale_for_frame(frame, pointer, |point| {
        if point.x >= 0.0 && point.x < 1800.0 {
            Ok(2.0)
        } else {
            Err(PlatformError::NotFound)
        }
    })
    .unwrap();
    assert_eq!(scale, 2.0);
    assert_eq!((pointer.x - frame.origin.x) * scale, 160.0);
    let frame = RectLogical::new(
        PointLogical::new(1600.0, 100.0),
        SizeLogical::new(400.0, 300.0),
    );
    let scale = scale_for_frame(frame, pointer, |point| {
        Ok(if point.x >= 1800.0 { 1.0 } else { 2.0 })
    })
    .unwrap();
    assert_eq!(
        scale, 1.0,
        "an on-screen centre retains its source display scale"
    );
}
use crosspane_types::geom::SizeLogical;

fn sample(id: u64, pid: i32, x: f64, y: f64) -> (WindowFact, PointLogical) {
    (
        WindowFact {
            window: WindowId(id),
            pid,
            frame: RectLogical::new(PointLogical::new(x, y), SizeLogical::new(400.0, 300.0)),
            scale: 2.0,
        },
        PointLogical::new(x + 80.0, y + 12.0),
    )
}

#[test]
fn recorded_textedit_and_safari_titlebar_moves_fire() {
    for (id, pid) in [(123, 98126), (456, 8685)] {
        let mut detector = Detector::default();
        let (window, point) = sample(id, pid, 300.0, 100.0);
        assert!(!detector.sample(window, point));
        let (window, point) = sample(id, pid, 304.0, 100.0);
        assert!(detector.sample(window, point));
    }
}

#[test]
fn drag_selection_and_static_drags_never_fire() {
    let mut detector = Detector::default();
    let (window, point) = sample(123, 98126, 300.0, 100.0);
    for dx in [0.0, 0.0, 5.0, 10.0, 15.0] {
        assert!(!detector.sample(
            window,
            point + crosspane_types::geom::VectorLogical::new(dx, 0.0)
        ));
    }
}

#[test]
fn left_and_top_edge_resizes_never_fire() {
    // Resizing by the left (or top) edge moves the origin with the pointer, so the offset stays
    // constant; only the changing size tells it from a move.
    let mut detector = Detector::default();
    for dx in [0.0, 5.0, 10.0, 15.0, 20.0] {
        let window = WindowFact {
            window: WindowId(123),
            pid: 98126,
            frame: RectLogical::new(
                PointLogical::new(300.0 + dx, 100.0 + dx),
                SizeLogical::new(400.0 - dx, 300.0 - dx),
            ),
            scale: 2.0,
        };
        assert!(!detector.sample(window, PointLogical::new(300.0 + dx, 150.0 + dx)));
    }
}

#[test]
fn window_and_pid_changes_reset_detection() {
    let mut detector = Detector::default();
    let (window, point) = sample(123, 98126, 300.0, 100.0);
    assert!(!detector.sample(window, point));
    let (window, point) = sample(456, 8685, 310.0, 100.0);
    assert!(!detector.sample(window, point));
    let (window, point) = sample(456, 8685, 315.0, 100.0);
    assert!(detector.sample(window, point));
    let (window, point) = sample(456, 9999, 320.0, 100.0);
    assert!(!detector.sample(window, point));
}

#[test]
fn slow_stable_move_accumulates_four_points_but_offset_jump_resets() {
    let mut detector = Detector::default();
    for dx in [0.0, 1.0, 2.0, 3.0, 4.0] {
        let (window, point) = sample(123, 98126, 300.0 + dx, 100.0);
        assert_eq!(detector.sample(window, point), dx >= 4.0);
    }
    let (window, mut point) = sample(123, 98126, 310.0, 100.0);
    point.y += 2.0;
    assert!(!detector.sample(window, point));
}

#[test]
fn idle_clear_emits_nothing_and_tracked_clear_releases_once() {
    let p = portal();
    let mut monitor = Move::default();
    assert!(monitor.clear(MonoTime::ZERO).is_empty());
    for x in [480.0, 485.0] {
        let (window, _) = sample(123, 98126, x - 80.0, 18.0);
        monitor.sample(Some(window), CGPoint::new(x, 30.0), &[p]);
    }
    monitor.update(&[p], &[(PortalId(1), 0.15)], MonoTime::ZERO);
    assert_eq!(monitor.clear(MonoTime::ZERO).len(), 1);
    assert!(monitor.clear(MonoTime::ZERO).is_empty());
}

fn portal() -> Portal {
    Portal {
        portal: crosspane_platform::CapturePortal {
            id: PortalId(1),
            display: crosspane_types::id::DisplayId(1),
            edge: Edge::Right,
            from: 0.0,
            to: 400.0,
        },
        display: super::super::Display {
            id: crosspane_types::id::DisplayId(1),
            scale: 2.0,
            bounds: CGRect::new(
                CGPoint::new(0.0, 0.0),
                objc2_core_foundation::CGSize::new(500.0, 400.0),
            ),
        },
    }
}

#[test]
fn lookup_is_event_driven_held_and_portal_bounded_across_the_whole_display() {
    let monitor = Move::default();
    let p = portal();
    assert!(!monitor.should_lookup(true, &[], CGPoint::new(500.0, 30.0)));
    assert!(!monitor.should_lookup(false, &[p], CGPoint::new(500.0, 30.0)));
    assert!(monitor.should_lookup(true, &[p], CGPoint::new(435.0, 30.0)));
    assert!(monitor.should_lookup(true, &[p], CGPoint::new(436.0, 30.0)));
}

#[test]
fn drag_edge_rate_grab_history_and_release_are_correlated() {
    let p = portal();
    let mut monitor = Move::default();
    for x in [440.0, 445.0, 450.0, 499.0] {
        let (window, _) = sample(123, 98126, x - 80.0, 18.0);
        monitor.sample(Some(window), CGPoint::new(x, 30.0), &[p]);
    }
    let hits = [(PortalId(1), 0.15)];
    let at = MonoTime::from_nanos(100_000_000);
    let events = monitor.update(&[p], &hits, at);
    assert!(
        matches!(events.as_slice(), [CaptureEvent::DragAtEdge { window: WindowId(123), grab, .. }] if *grab == PointDevice::new(160.0, 24.0))
    );
    for dt in [1, 19] {
        assert!(
            monitor
                .update(
                    &[p],
                    &hits,
                    MonoTime::from_nanos(at.as_nanos() + dt * 1_000_000)
                )
                .is_empty()
        );
    }
    assert_eq!(
        monitor
            .update(
                &[p],
                &hits,
                MonoTime::from_nanos(at.as_nanos() + 20_000_000)
            )
            .len(),
        1
    );
    let (_, before) = monitor.at_edge(PortalId(1)).unwrap();
    assert_eq!(before.unwrap().origin.x, 370.0);
    // Empty outward hits while pinned at the edge retain the drag. Leave the edge band.
    monitor.pointer = CGPoint::new(450.0, 30.0);
    assert!(matches!(
        monitor
            .update(&[p], &[], MonoTime::from_nanos(at.as_nanos() + 21_000_000))
            .as_slice(),
        [CaptureEvent::EdgeReleased {
            portal: PortalId(1),
            ..
        }]
    ));
    assert!(monitor.at_edge(PortalId(1)).is_none());
}

#[test]
fn jumped_in_history_is_absent_and_move_end_releases_the_edge() {
    let p = portal();
    let mut monitor = Move::default();
    for x in [480.0, 485.0] {
        let (window, _) = sample(123, 98126, x - 80.0, 18.0);
        monitor.sample(Some(window), CGPoint::new(x, 30.0), &[p]);
    }
    let at = MonoTime::from_nanos(100_000_000);
    monitor.update(&[p], &[(PortalId(1), 0.15)], at);
    assert_eq!(monitor.at_edge(PortalId(1)).unwrap().1, None);
    assert!(matches!(
        monitor.clear(at).as_slice(),
        [CaptureEvent::EdgeReleased { .. }]
    ));
    assert!(monitor.at_edge(PortalId(1)).is_none());
}

#[test]
fn grab_uses_source_window_scale_when_portal_is_on_a_different_scale() {
    let p = portal();
    let mut monitor = Move::default();
    for x in [480.0, 485.0] {
        let (mut window, _) = sample(123, 98126, x - 80.0, 18.0);
        window.scale = 1.0;
        monitor.sample(Some(window), CGPoint::new(x, 30.0), &[p]);
    }
    let events = monitor.update(&[p], &[(PortalId(1), 0.15)], MonoTime::ZERO);
    assert!(
        matches!(events.as_slice(), [CaptureEvent::DragAtEdge { grab, .. }] if *grab == PointDevice::new(80.0, 12.0))
    );
}

#[test]
fn tiling_decision_accepts_only_changed_native_tiles_within_500ms() {
    let start = Instant::now();
    let before = sample(123, 98126, 300.0, 100.0).0.frame;
    let work = RectLogical::new(
        PointLogical::new(0.0, 30.0),
        SizeLogical::new(1800.0, 1130.0),
    );
    let tile = RectLogical::new(
        PointLogical::new(899.0, 30.0),
        SizeLogical::new(902.0, 1130.0),
    );
    assert!(tiled(
        before,
        tile,
        work,
        start,
        start + Duration::from_millis(500)
    ));
    assert!(!tiled(
        before,
        tile,
        work,
        start,
        start + Duration::from_millis(501)
    ));
    assert!(!tiled(
        before,
        before.translate(crosspane_types::geom::VectorLogical::new(50.0, 0.0)),
        work,
        start,
        start
    ));
    assert!(!tiled(
        before,
        RectLogical::new(
            PointLogical::new(700.0, 400.0),
            SizeLogical::new(450.0, 330.0)
        ),
        work,
        start,
        start
    ));
}

#[test]
fn native_move_model_reports_current_coherent_device_geometry_without_portals() {
    let mut monitor = Move::default();
    for x in [300.0, 305.0] {
        let (window, pointer) = sample(123, 7, x, 100.0);
        monitor.sample(Some(window), CGPoint::new(pointer.x, pointer.y), &[]);
    }
    assert!(
        matches!(monitor.native(MonoTime::ZERO).as_slice(), [CaptureEvent::NativeMove { window: WindowId(123), grab, size, .. }]
        if *grab == PointDevice::new(160.0, 24.0) && *size == PixelSize::new(800, 600))
    );
    let (window, mut pointer) = sample(123, 7, 310.0, 100.0);
    pointer.y += 0.5;
    monitor.sample(Some(window), CGPoint::new(pointer.x, pointer.y), &[]);
    assert!(
        matches!(monitor.native(MonoTime::ZERO).as_slice(), [CaptureEvent::NativeMove { grab, .. }]
        if *grab == PointDevice::new(160.0, 25.0))
    );
    assert!(matches!(
        monitor.clear(MonoTime::ZERO).as_slice(),
        [CaptureEvent::NativeMoveEnded {
            window: WindowId(123),
            ..
        }]
    ));
    assert!(monitor.clear(MonoTime::ZERO).is_empty());
}

#[test]
fn native_move_model_latch_break_or_lost_window_ends_once() {
    for reason in ["window", "pid", "resize", "offset", "lost"] {
        let mut monitor = Move::default();
        for x in [300.0, 305.0] {
            let (window, pointer) = sample(123, 7, x, 100.0);
            monitor.sample(Some(window), CGPoint::new(pointer.x, pointer.y), &[]);
        }
        monitor.native(MonoTime::ZERO);
        let (mut window, mut pointer) = sample(123, 7, 310.0, 100.0);
        match reason {
            "window" => window.window = WindowId(124),
            "pid" => window.pid = 8,
            "resize" => window.frame.size.width += 1.0,
            "offset" => pointer.y += 2.0,
            _ => {}
        }
        monitor.sample(
            (reason != "lost").then_some(window),
            CGPoint::new(pointer.x, pointer.y),
            &[],
        );
        assert!(matches!(
            monitor.native(MonoTime::ZERO).as_slice(),
            [CaptureEvent::NativeMoveEnded {
                window: WindowId(123),
                ..
            }]
        ));
        assert!(monitor.native(MonoTime::ZERO).is_empty());
        assert!(monitor.clear(MonoTime::ZERO).is_empty());
    }
}

#[test]
fn native_move_model_invalid_device_geometry_ends_instead_of_casting() {
    for scale in [
        f64::NAN,
        f64::INFINITY,
        0.0,
        -1.0,
        0.0001,
        f64::from(u32::MAX),
    ] {
        let mut monitor = Move::default();
        for x in [300.0, 305.0] {
            let (window, pointer) = sample(123, 7, x, 100.0);
            monitor.sample(Some(window), CGPoint::new(pointer.x, pointer.y), &[]);
        }
        monitor.native(MonoTime::ZERO);
        let (mut window, pointer) = sample(123, 7, 310.0, 100.0);
        window.scale = scale;
        monitor.sample(Some(window), CGPoint::new(pointer.x, pointer.y), &[]);
        assert!(matches!(
            monitor.native(MonoTime::ZERO).as_slice(),
            [CaptureEvent::NativeMoveEnded { .. }]
        ));
        assert!(monitor.native(MonoTime::ZERO).is_empty());
    }
}
