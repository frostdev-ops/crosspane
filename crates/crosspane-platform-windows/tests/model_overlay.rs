use crosspane_platform::{Overlay, OverlayAnchor, OverlayEvent, OverlayId, Rgb8};
use crosspane_platform_windows::model::{
    geometry::{DisplayIds, MonitorProbe, displays},
    overlay::{OverlayModel, compose, follow_current, placement, text_line},
};

fn monitor() -> MonitorProbe {
    MonitorProbe {
        device_path: "owned-test-monitor".into(),
        name: "owned-test".into(),
        rc_monitor: [-1920, 0, 0, 1080],
        rc_work: [-1920, 0, 0, 1040],
        primary: true,
        dpi: 144,
        refresh_millihz: 60_000,
        edid: None,
        twin: false,
        quarter_turns: 0,
    }
}

#[test]
fn every_anchor_uses_display_local_conversion_and_work_area() {
    let monitor = monitor();
    let layout = displays(std::slice::from_ref(&monitor), &mut DisplayIds::default()).unwrap();
    let display = &layout.displays[0];
    for (anchor, expected) in [
        (OverlayAnchor::TopCenter, [-1140, 36]),
        (OverlayAnchor::TopRight, [-396, 36]),
        (OverlayAnchor::BottomRight, [-396, 944]),
        (OverlayAnchor::Center, [-1140, 490]),
    ] {
        let overlay = Overlay {
            display: display.id,
            anchor,
            text: "Input → owned peer".into(),
            accent: Rgb8 {
                r: 10,
                g: 20,
                b: 30,
            },
        };
        let actual = placement(&overlay, &monitor, &display.geometry, 200.0).unwrap();
        assert_eq!(actual.origin, expected);
        assert_eq!(actual.size, [360, 60]);
    }
}

#[test]
fn invalid_geometry_and_oversized_content_fail_or_clamp_without_overflow() {
    let monitor = monitor();
    let layout = displays(std::slice::from_ref(&monitor), &mut DisplayIds::default()).unwrap();
    let display = &layout.displays[0];
    let overlay = Overlay {
        display: display.id,
        anchor: OverlayAnchor::TopRight,
        text: "x".into(),
        accent: Rgb8 { r: 1, g: 2, b: 3 },
    };
    assert!(placement(&overlay, &monitor, &display.geometry, f64::NAN).is_err());
    let wide = placement(&overlay, &monitor, &display.geometry, 99_999.0).unwrap();
    assert!(wide.origin[0] >= monitor.rc_work[0]);
    assert!(wide.origin[0] + wide.size[0] <= monitor.rc_work[2]);
    assert_eq!(text_line(&"a".repeat(129)).chars().count(), 128);
    assert!(text_line("a\0\nb").chars().all(|c| !c.is_control()));
}

#[test]
fn accepted_show_needs_fresh_presentation_and_hide_retires_it() {
    let mut state = OverlayModel::default();
    let id = OverlayId(7);
    let first = state.show(id, 10).unwrap();
    assert!(state.replay().is_empty());
    let replacement = state.show(id, 20).unwrap();
    assert!(state.observe(id, first, true, 21).is_empty());
    assert_eq!(
        state.observe(id, replacement, true, 21),
        vec![OverlayEvent::Visible(id)]
    );
    assert_eq!(state.replay(), vec![OverlayEvent::Visible(id)]);
    state.hide(id).unwrap();
    state.hide(OverlayId(99)).unwrap();
    assert!(state.observe(id, replacement, true, 22).is_empty());
    assert!(state.replay().is_empty());
}

#[test]
fn loss_timeout_and_shutdown_never_later_publish_stale_visible() {
    let mut state = OverlayModel::default();
    let id = OverlayId(3);
    let revision = state.show(id, 0).unwrap();
    assert_eq!(state.expire(500), vec![OverlayEvent::Unavailable(id)]);
    assert!(state.observe(id, revision, true, 501).is_empty());
    let revision = state.show(id, 600).unwrap();
    assert_eq!(
        state.observe(id, revision, true, 601),
        vec![OverlayEvent::Visible(id)]
    );
    assert_eq!(
        state.observe(id, revision, false, 602),
        vec![OverlayEvent::Unavailable(id)]
    );
    assert!(state.observe(id, revision, true, 603).is_empty());
    let revision = state.show(id, 700).unwrap();
    assert_eq!(state.shutdown(), vec![OverlayEvent::Unavailable(id)]);
    assert!(state.observe(id, revision, true, 701).is_empty());
    assert!(state.show(id, 800).is_err());
}

#[test]
fn parity_composition_is_premultiplied_with_transparent_corners_and_literal_accent() {
    let mut pixels = vec![0u8; 100 * 40 * 4];
    let text = (20 * 100 + 30) * 4;
    pixels[text..text + 3].fill(128);
    compose(
        &mut pixels,
        [100, 40],
        1.0,
        Rgb8 {
            r: 250,
            g: 150,
            b: 50,
        },
    )
    .unwrap();
    assert_eq!(&pixels[..4], &[0, 0, 0, 0]);
    let bar = (20 * 100 + 9) * 4;
    assert_eq!(&pixels[bar..bar + 4], &[50, 150, 250, 255]);
    let background = (20 * 100 + 50) * 4;
    assert_eq!(&pixels[background..background + 4], &[50, 37, 28, 230]);
    assert_eq!(&pixels[text..text + 4], &[153, 146, 142, 243]);
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[..3].iter().all(|c| *c <= p[3]))
    );
    assert!(compose(&mut pixels[..3], [100, 40], 1.0, Rgb8 { r: 0, g: 0, b: 0 }).is_err());
}

#[test]
fn workspace_follow_checks_after_owned_move_and_fails_closed_on_each_api_error() {
    use std::cell::RefCell;
    let calls = RefCell::new(Vec::new());
    let mut states = [false, true].into_iter();
    follow_current(
        || {
            calls.borrow_mut().push("membership");
            Ok(states.next().unwrap())
        },
        || {
            calls.borrow_mut().push("owned-move");
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(*calls.borrow(), ["membership", "owned-move", "membership"]);
    assert!(
        follow_current(
            || Err(crosspane_platform::PlatformError::Timeout),
            || panic!("unknown membership must not move")
        )
        .is_err()
    );
    assert!(
        follow_current(
            || Ok(false),
            || Err(crosspane_platform::PlatformError::NotFound)
        )
        .is_err()
    );
    assert!(follow_current(|| Ok(false), || Ok(())).is_err());
    follow_current(|| Ok(true), || panic!("current window must not move")).unwrap();
}
