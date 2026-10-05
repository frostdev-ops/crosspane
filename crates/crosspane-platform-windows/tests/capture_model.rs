use crosspane_platform::{CaptureEvent, CaptureId, CapturePortal, Edge, PortalId};
use crosspane_platform_windows::model::capture::*;
use crosspane_platform_windows::model::hook::{HookState, MouseIn, MouseKind};
use crosspane_types::{
    geom::{PixelRect, euclid::point2},
    id::DisplayId,
    input::LockKeys,
    time::MonoTime,
};

#[test]
fn keyboard_loss_with_healthy_pumping_and_mouse_is_bounded() {
    let mut p = Probe::new(23, 0).unwrap();
    assert_eq!(p.poll(0), ProbeAction::Pair);
    assert_eq!(p.submitted(2), ProbeAction::None);
    assert_eq!(p.poll(249), ProbeAction::None);
    assert_eq!(p.poll(250), ProbeAction::Lost);
    assert_eq!(p.poll(750), ProbeAction::None); // never a second outstanding pair
}

#[test]
fn probe_requires_both_edges_exact_nonce_scan_vk_and_injected_flag() {
    let mut p = Probe::new(23, 0).unwrap();
    assert_eq!(p.poll(0), ProbeAction::Pair);
    p.submitted(2);
    assert!(!p.acknowledge(24, F24_SCAN, F24_VK, true, false));
    assert!(!p.acknowledge(23, F24_SCAN, F24_VK, false, false));
    assert!(!p.acknowledge(23, 1, F24_VK, true, false));
    assert!(!p.acknowledge(23, F24_SCAN, 1, true, false));
    assert!(p.acknowledge(23, F24_SCAN, F24_VK, true, false));
    assert_eq!(p.poll(500), ProbeAction::Lost);
}

#[test]
fn acknowledged_probe_waits_250ms_and_never_overlaps_pairs() {
    let mut p = Probe::new(23, 0).unwrap();
    assert_eq!(p.poll(0), ProbeAction::Pair);
    p.submitted(2);
    assert_eq!(p.poll(249), ProbeAction::None);
    for up in [false, true] {
        assert!(p.acknowledge(23, F24_SCAN, F24_VK, true, up));
    }
    assert_eq!(p.poll(251), ProbeAction::None);
    assert_eq!(p.poll(500), ProbeAction::None);
    assert_eq!(p.poll(501), ProbeAction::Pair);
}

#[test]
fn partial_down_is_lost_and_only_matching_up_is_retried_three_times() {
    let mut p = Probe::new(23, 0).unwrap();
    assert_eq!(p.poll(0), ProbeAction::Pair);
    assert_eq!(p.submitted(1), ProbeAction::Lost);
    assert_eq!(p.poll(0), ProbeAction::Up);
    p.up_submitted(0);
    assert_eq!(p.poll(499), ProbeAction::None);
    assert_eq!(p.poll(500), ProbeAction::Up);
    p.up_submitted(0);
    assert_eq!(p.poll(1000), ProbeAction::Up);
    p.up_submitted(0);
    assert_eq!(p.poll(1500), ProbeAction::CleanupUnverified);
    assert_eq!(p.poll(2000), ProbeAction::None);
}

#[test]
fn zero_submission_is_lost_without_an_up_or_second_pair() {
    let mut p = Probe::new(23, 0).unwrap();
    p.poll(0);
    assert_eq!(p.submitted(0), ProbeAction::Lost);
    assert_eq!(p.poll(500), ProbeAction::None);
}

#[test]
fn successful_matching_up_settles_partial_submission() {
    let mut p = Probe::new(23, 0).unwrap();
    p.poll(0);
    p.submitted(1);
    assert_eq!(p.poll(0), ProbeAction::Up);
    p.up_submitted(1);
    assert_eq!(p.poll(500), ProbeAction::None);
}

#[test]
fn gate_epoch_catches_close_reopen_and_pump_is_not_keyboard_attestation() {
    let mut w = Watchdog::new(7);
    assert!(!w.lost(0, true, 7, 0, 0, 0));
    assert!(w.lost(1, true, 9, 1, 0, 0));
    let mut w = Watchdog::new(7);
    assert!(w.lost(100, true, 7, 0, 0, 0));
}

#[test]
fn raw_mouse_without_hook_observation_aborts_both_after_short_window() {
    let mut w = Watchdog::new(7);
    assert!(!w.lost(1, true, 7, 1, 0, 1));
    assert!(!w.lost(100, true, 7, 100, 0, 1));
    assert!(w.lost(101, true, 7, 101, 0, 1));
    let mut w = Watchdog::new(7);
    assert!(!w.lost(1, true, 7, 1, 0, 1));
    assert!(!w.lost(50, true, 7, 50, 1, 1));
    assert!(!w.lost(101, true, 7, 101, 1, 1));
}

#[test]
fn constant_time_ledger_preserves_local_held_ups_and_suppressed_tails() {
    assert_eq!(key_transition(1, true, true), (0, false));
    assert_eq!(key_transition(1, true, false), (1, false));
    assert_eq!(key_transition(0, true, false), (2, true));
    assert_eq!(key_transition(2, false, false), (2, true));
    assert_eq!(key_transition(2, false, true), (0, true));
    assert_eq!(key_transition(0, false, false), (1, false));
    assert_eq!(key_slot(256, false, 1), None);
    assert_ne!(key_slot(0, false, 1), key_slot(0, false, 2));
    assert_ne!(key_slot(1, false, 1), key_slot(1, true, 1));
}

#[test]
fn every_partial_acquisition_unwinds_and_never_commits_late() {
    for failed in [
        Resource::KeyboardHook,
        Resource::MouseHook,
        Resource::MouseRaw,
        Resource::CursorWindow,
        Resource::PointerClip,
    ] {
        let mut active = Vec::new();
        let result = acquire(|r, start| {
            if start {
                active.push(r);
                if r == failed {
                    return Err("cancelled generation or native failure");
                }
            } else {
                assert_eq!(active.pop(), Some(r));
            }
            Ok(())
        });
        assert!(result.is_err());
        assert!(active.is_empty());
    }
}

#[test]
fn acknowledged_whole_batch_does_not_authorize_an_extra_cleanup_up() {
    let mut p = Probe::new(23, 0).unwrap();
    p.poll(0);
    p.submitted(2);
    p.stop(1);
    assert_eq!(p.poll(1), ProbeAction::None);
}

#[test]
fn loss_just_after_acknowledgement_is_detected_within_500ms_total() {
    let mut p = Probe::new(23, 0).unwrap();
    assert_eq!(p.poll(0), ProbeAction::Pair);
    p.submitted(2);
    for up in [false, true] {
        p.acknowledge(23, F24_SCAN, F24_VK, true, up);
    }
    assert_eq!(p.poll(0), ProbeAction::None);
    // Hook disappears at t=1; pumping and mouse observations can remain healthy.
    assert_eq!(p.poll(249), ProbeAction::None);
    assert_eq!(p.poll(250), ProbeAction::Pair);
    p.submitted(2);
    assert_eq!(p.poll(499), ProbeAction::None);
    assert_eq!(p.poll(500), ProbeAction::Lost);
}

#[test]
fn teardown_does_not_abandon_owed_up_or_accelerate_failed_retry_budget() {
    let mut p = Probe::new(23, 0).unwrap();
    p.poll(0);
    p.submitted(1);
    assert_eq!(p.poll(0), ProbeAction::Up);
    p.up_submitted(0);
    p.stop(10);
    assert!(p.cleanup_pending());
    assert_eq!(p.poll(10), ProbeAction::None);
    assert_eq!(p.poll(500), ProbeAction::Up);
    p.up_submitted(1);
    assert!(!p.cleanup_pending());
}

#[test]
fn abort_between_activation_precheck_and_publish_cannot_activate_late() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let control = AtomicU64::new(4);
    let ticket = reserve(&control).unwrap();
    assert!(!publish(&control, ticket, || {
        cancel(&control);
    }));
    assert_eq!(control.load(Ordering::Acquire) & 3, IDLE);
    let next = reserve(&control).unwrap();
    assert_ne!(ticket, next);
    assert!(publish(&control, next, || {}));
    assert_eq!(control.load(Ordering::Acquire) & 3, ACTIVE);
}

#[test]
fn old_physical_tail_never_forwards_to_a_reused_capture_id() {
    assert_eq!(ledger_token(2, Some(9), 5), Some(5));
    assert_eq!(ledger_token(2, None, 5), None);
    assert_eq!(ledger_token(0, Some(9), 5), Some(9));
}

#[test]
fn entering_portal_is_consumed_before_started_without_cancelling_crossing() {
    let mut hook = HookState::new();
    hook.set_portals(
        vec![(
            CapturePortal {
                id: PortalId(1),
                display: DisplayId(1),
                edge: Edge::Right,
                from: 0.0,
                to: 100.0,
            },
            PixelRect::new(point2(99, 0), point2(100, 100)),
        )],
        MonoTime::ZERO,
    )
    .unwrap();
    let decision = hook.on_mouse(MouseIn {
        kind: MouseKind::Move,
        pt: (99, 50),
        delta: Some((1.0, 0.0)),
        dragged: false,
        buttons_down: None,
        injected: false,
        ours: false,
        at: MonoTime::ZERO,
    });
    assert!(matches!(
        decision.events.as_slice(),
        [CaptureEvent::EdgePressed { .. }]
    ));
    let (_, events) = hook
        .native_begin(
            CaptureId(1),
            PortalId(1),
            &[],
            &[false; 256],
            LockKeys::default(),
            MonoTime::ZERO,
        )
        .unwrap();
    assert_eq!(events, vec![CaptureEvent::Started { id: CaptureId(1) }]);
}
