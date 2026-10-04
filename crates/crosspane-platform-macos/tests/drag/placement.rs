use super::*;

#[test]
fn restore_placement_clamps_current_ax_size_despite_stale_quartz() {
    let current = rect(200.0, 100.0, 800.0, 600.0);
    let mut placed = None;
    placement_with(
        rect(200.0, 100.0, 400.0, 300.0),
        rect(0.0, 0.0, 1800.0, 1100.0),
        rect(0.0, 30.0, 1800.0, 1070.0),
        1.0,
        PointDevice::new(1600.0, 1000.0),
        || Ok(current),
        || Ok(true),
        |frame| {
            placed = Some(frame);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(placed.unwrap(), rect(1000.0, 500.0, 800.0, 600.0));
}

#[test]
fn twin_restore_placement_uses_conservative_lookup_for_title_and_missing_target() {
    let differing_title = placement_lookup_with(
        true,
        || Err(PlatformError::NotFound),
        || Ok("exact frame with differing title"),
    );
    assert!(differing_title.is_ok());
    let unrelated = placement_lookup_with(
        true,
        || Ok("lone unrelated AX candidate"),
        || Err(PlatformError::NotFound),
    );
    assert!(matches!(unrelated, Err(PlatformError::NotFound)));
}

#[test]
fn restore_placement_refuses_target_that_leaves_visible_space_before_write() {
    let frame = rect(100.0, 100.0, 400.0, 300.0);
    let mut wrote = false;
    let result = placement_with(
        frame,
        rect(0.0, 0.0, 1800.0, 1100.0),
        rect(0.0, 30.0, 1800.0, 1070.0),
        1.0,
        PointDevice::new(40.0, 50.0),
        || Ok(frame),
        || Ok(false),
        |_| {
            wrote = true;
            Ok(())
        },
    );
    assert!(matches!(result, Err(PlatformError::NotFound)));
    assert!(!wrote);
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> RectLogical {
    RectLogical::new(PointLogical::new(x, y), SizeLogical::new(w, h))
}

#[test]
fn restore_at_scales_relative_origin_clamps_visible_frame_and_keeps_size() {
    let original = rect(1.0, 2.0, 300.0, 200.0);
    let bounds = rect(-1000.0, 10.0, 1000.0, 800.0);
    let visible = rect(-1000.0, 40.0, 1000.0, 740.0);
    assert_eq!(
        placement_frame(
            original,
            bounds,
            visible,
            2.0,
            PointDevice::new(400.0, 200.0)
        )
        .unwrap(),
        rect(-800.0, 110.0, 300.0, 200.0)
    );
    assert_eq!(
        placement_frame(
            original,
            bounds,
            visible,
            2.0,
            PointDevice::new(-200.0, -200.0)
        )
        .unwrap(),
        rect(-1000.0, 40.0, 300.0, 200.0)
    );
    assert_eq!(
        placement_frame(
            original,
            bounds,
            visible,
            2.0,
            PointDevice::new(5000.0, 5000.0)
        )
        .unwrap(),
        rect(-300.0, 580.0, 300.0, 200.0)
    );
}

#[test]
fn restore_at_oversize_anchors_at_work_origin_and_invalid_geometry_refuses() {
    let frame = rect(0.0, 0.0, 1500.0, 900.0);
    let bounds = rect(0.0, 0.0, 1000.0, 800.0);
    let visible = rect(0.0, 30.0, 1000.0, 750.0);
    assert_eq!(
        placement_frame(frame, bounds, visible, 1.0, PointDevice::new(100.0, 100.0)).unwrap(),
        rect(0.0, 30.0, 1500.0, 900.0)
    );
    for scale in [0.0, -1.0, f64::NAN] {
        assert!(placement_frame(frame, bounds, visible, scale, PointDevice::zero()).is_err());
    }
    assert!(
        placement_frame(
            frame,
            bounds,
            visible,
            1.0,
            PointDevice::new(f64::INFINITY, 0.0)
        )
        .is_err()
    );
}

#[test]
fn restore_at_restores_first_preserves_restore_failure_and_accepts_placement_failure() {
    let calls = std::cell::RefCell::new(Vec::new());
    assert!(
        restore_and_place(
            || {
                calls.borrow_mut().push("restore");
                Ok(())
            },
            || {
                calls.borrow_mut().push("place");
                Err(PlatformError::NotFound)
            }
        )
        .is_ok()
    );
    assert_eq!(*calls.borrow(), ["restore", "place"]);
    calls.borrow_mut().clear();
    assert!(matches!(
        restore_and_place(
            || {
                calls.borrow_mut().push("restore");
                Err(PlatformError::Timeout)
            },
            || { panic!("failed restoration cannot place or retire journal material") }
        ),
        Err(PlatformError::Timeout)
    ));
    assert_eq!(*calls.borrow(), ["restore"]);
}
