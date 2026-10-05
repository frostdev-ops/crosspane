//! Exact pure HookState tail contracts; no native hook or input is installed.
#![allow(clippy::unwrap_used)]

use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, Edge, EndReason, PlatformError, PortalId,
};
#[path = "../src/model/hook.rs"]
#[allow(dead_code)]
mod hook;
use crosspane_types::{
    geom::{PixelRect, euclid::point2},
    hid::MouseButton,
    id::DisplayId,
    input::LockKeys,
    time::MonoTime,
};
use hook::{DragSettlementResult, HookState, MouseIn, MouseKind};

fn at(ms: u64) -> MonoTime {
    MonoTime::from_nanos(ms * 1_000_000)
}

fn mouse(down: bool, injected: bool) -> MouseIn {
    MouseIn {
        kind: MouseKind::Button { n: 0, down },
        pt: (99, 50),
        delta: None,
        dragged: false,
        buttons_down: None,
        injected,
        ours: false,
        at: at(40),
    }
}

fn physical_down() -> HookState {
    let mut state = HookState::new();
    state
        .set_portals(
            vec![(
                CapturePortal {
                    id: PortalId(1),
                    display: DisplayId(7),
                    edge: Edge::Right,
                    from: 0.0,
                    to: 100.0,
                },
                PixelRect::new(point2(99, 0), point2(100, 100)),
            )],
            at(0),
        )
        .unwrap();
    let down = state.on_mouse(mouse(true, false));
    assert!(!down.suppress);
    assert!(down.events.is_empty());
    state
}

fn activate(state: &mut HookState, id: u64) -> Vec<CaptureEvent> {
    state.mark_drag_dispatch(CaptureId(id)).unwrap();
    assert!(!state.drag_tagged_up(1000 + id as usize).suppress);
    state
        .record_drag_settlement(CaptureId(id), DragSettlementResult::Accepted)
        .unwrap();
    let mut buttons = [false; 256];
    buttons[0] = true;
    state
        .native_begin_drag(
            CaptureId(id),
            PortalId(1),
            MouseButton::PRIMARY,
            &[],
            &buttons,
            LockKeys::default(),
            at(20),
        )
        .unwrap()
        .1
}

#[test]
fn accepted_settlement_reports_exactly_one_physical_up_after_started_and_no_down() {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    assert_eq!(
        activate(&mut state, 1),
        vec![CaptureEvent::Started { id: CaptureId(1) }]
    );
    let up = state.on_mouse(mouse(false, false));
    assert!(up.suppress);
    assert_eq!(
        up.events,
        vec![CaptureEvent::Button {
            button: MouseButton::PRIMARY,
            down: false,
            at: at(40)
        }]
    );
    assert!(!state.drag_tail_pending());
    assert!(state.on_mouse(mouse(false, false)).events.is_empty());
}

#[test]
fn reentrant_physical_up_then_known_zero_is_settled_without_pending_tag_or_capture() {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    state.mark_drag_dispatch(CaptureId(1)).unwrap();
    // Revised hook-event ordering: the first physical UP passes BEFORE the SendInput report.
    let up = state.on_mouse(mouse(false, false));
    assert!(!up.suppress);
    assert!(up.events.is_empty());
    state
        .record_drag_settlement(CaptureId(1), DragSettlementResult::KnownZero)
        .unwrap();
    assert!(state.cancel_unsubmitted_drag(CaptureId(1)).is_err());
    let mut buttons = [false; 256];
    buttons[0] = true;
    assert!(
        state
            .native_begin_drag(
                CaptureId(1),
                PortalId(1),
                MouseButton::PRIMARY,
                &[],
                &buttons,
                LockKeys::default(),
                at(50)
            )
            .is_err()
    );
    assert!(state.end(EndReason::Lost, at(51)).is_empty());
    assert!(!state.drag_tail_pending()); // The real physical tail was consumed, not restored.
}

#[test]
fn accepted_tail_survives_abort_end_and_drop_equivalent_without_post_ended_event() {
    for reason in [EndReason::Aborted, EndReason::Requested, EndReason::Lost] {
        let mut state = physical_down();
        state
            .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
            .unwrap();
        activate(&mut state, 1);
        assert_eq!(
            state.end(reason, at(30)),
            vec![CaptureEvent::Ended {
                id: CaptureId(1),
                reason
            }]
        );
        assert!(state.end(reason, at(31)).is_empty());
        assert!(state.drag_tail_pending());
        assert!(!state.all_released());
        let up = state.on_mouse(mouse(false, false));
        assert!(up.suppress);
        assert!(up.events.is_empty());
        assert!(!state.drag_tail_pending());
        assert!(state.all_released());
    }
}

#[test]
fn activation_failure_and_uncertain_submission_preserve_tail_before_started() {
    for attempt_activation in [false, true] {
        let mut state = physical_down();
        state
            .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
            .unwrap();
        if attempt_activation {
            state.mark_drag_dispatch(CaptureId(1)).unwrap();
            assert!(!state.drag_tagged_up(1001).suppress);
            state
                .record_drag_settlement(CaptureId(1), DragSettlementResult::Accepted)
                .unwrap();
            let mut buttons = [false; 256];
            buttons[0] = true;
            assert!(matches!(
                state.native_begin_drag(
                    CaptureId(1),
                    PortalId(99),
                    MouseButton::PRIMARY,
                    &[],
                    &buttons,
                    LockKeys::default(),
                    at(20)
                ),
                Err(PlatformError::NotFound)
            ));
        } else {
            state.mark_drag_dispatch(CaptureId(1)).unwrap();
            state
                .record_drag_settlement(CaptureId(1), DragSettlementResult::Uncertain)
                .unwrap();
            assert!(!state.drag_tagged_up(1001).suppress);
        }
        assert!(state.end(EndReason::Aborted, at(30)).is_empty());
        assert!(state.drag_tail_pending());
        let up = state.on_mouse(mouse(false, false));
        assert!(up.suppress);
        assert!(up.events.is_empty());
        assert!(state.all_released());
    }
}

#[test]
fn proven_zero_submission_rolls_back_only_reservation_and_allows_original_up() {
    for dispatched in [false, true] {
        let mut state = physical_down();
        state
            .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
            .unwrap();
        if dispatched {
            state.mark_drag_dispatch(CaptureId(1)).unwrap();
            state
                .record_drag_settlement(CaptureId(1), DragSettlementResult::KnownZero)
                .unwrap();
        } else {
            state.cancel_unsubmitted_drag(CaptureId(1)).unwrap();
        }
        assert!(!state.drag_tail_pending());
        let up = state.on_mouse(mouse(false, false));
        assert!(!up.suppress);
        assert!(up.events.is_empty());
        assert!(state.all_released());
    }
}

#[test]
fn accepted_or_uncertain_cannot_be_rolled_back_by_generic_error_or_late_zero() {
    for outcome in [
        DragSettlementResult::Accepted,
        DragSettlementResult::Uncertain,
    ] {
        let mut state = physical_down();
        state
            .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
            .unwrap();
        state.mark_drag_dispatch(CaptureId(1)).unwrap();
        state.record_drag_settlement(CaptureId(1), outcome).unwrap();
        assert!(!state.drag_tagged_up(1001).suppress);
        assert!(state.cancel_unsubmitted_drag(CaptureId(1)).is_err());
        assert!(
            state
                .record_drag_settlement(CaptureId(1), DragSettlementResult::KnownZero)
                .is_err()
        );
        assert!(state.drag_tail_pending());
        assert!(state.on_mouse(mouse(false, false)).suppress);
        assert!(state.all_released());
    }
}

#[test]
fn dispatching_panic_state_is_already_protected_and_never_admits_capture() {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    state.mark_drag_dispatch(CaptureId(1)).unwrap();
    assert!(state.cancel_unsubmitted_drag(CaptureId(1)).is_err());
    let mut buttons = [false; 256];
    buttons[0] = true;
    assert!(
        state
            .native_begin_drag(
                CaptureId(1),
                PortalId(1),
                MouseButton::PRIMARY,
                &[],
                &buttons,
                LockKeys::default(),
                at(20)
            )
            .is_err()
    );
    assert!(state.drag_tail_pending());
    assert!(state.end(EndReason::Aborted, at(30)).is_empty());
    assert!(!state.on_mouse(mouse(false, false)).suppress);
    assert!(state.drag_tagged_up(1001).suppress);
}

#[test]
fn old_tail_cannot_be_retagged_or_cleared_by_new_attempt_or_snapshot() {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    assert!(
        state
            .protect_drag_tail(CaptureId(2), MouseButton::PRIMARY, 1002)
            .is_err()
    );
    assert!(state.cancel_unsubmitted_drag(CaptureId(2)).is_err());
    state.reconcile(&[], &[false; 256]);
    assert!(state.drag_tail_pending());
    assert!(!state.on_mouse(mouse(false, false)).suppress);
    assert!(state.all_released());
    state.on_mouse(mouse(true, false));
    state
        .protect_drag_tail(CaptureId(2), MouseButton::PRIMARY, 1002)
        .unwrap();
    assert_eq!(
        activate(&mut state, 2),
        vec![CaptureEvent::Started { id: CaptureId(2) }]
    );
}

#[test]
fn injected_and_ours_up_do_not_clear_physical_tail_or_report_release() {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    activate(&mut state, 1);
    for ours in [false, true] {
        let mut injected = mouse(false, true);
        injected.ours = ours;
        assert!(state.on_mouse(injected).events.is_empty());
        assert!(state.drag_tail_pending());
    }
    assert_eq!(state.on_mouse(mouse(false, false)).events.len(), 1);
    assert!(!state.drag_tail_pending());
}

#[test]
fn primary_only_admission_and_ordinary_begin_held_refusal_remain_unchanged() {
    let mut state = physical_down();
    assert!(matches!(
        state.protect_drag_tail(CaptureId(1), MouseButton::SECONDARY, 1001),
        Err(PlatformError::PointerButtonHeld)
    ));
    let mut secondary = mouse(true, false);
    secondary.kind = MouseKind::Button { n: 1, down: true };
    state.on_mouse(secondary);
    assert!(matches!(
        state.protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001),
        Err(PlatformError::PointerButtonHeld)
    ));
    assert!(!state.drag_tail_pending());
    let mut buttons = [false; 256];
    buttons[0] = true;
    assert!(matches!(
        state.native_begin(
            CaptureId(1),
            PortalId(1),
            &[],
            &buttons,
            LockKeys::default(),
            at(20)
        ),
        Err(PlatformError::PointerButtonHeld)
    ));
}

#[test]
fn physical_first_passes_and_only_matching_tagged_counterpart_is_swallowed() {
    for report in [
        DragSettlementResult::Accepted,
        DragSettlementResult::Uncertain,
    ] {
        let mut state = physical_down();
        state
            .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
            .unwrap();
        state.mark_drag_dispatch(CaptureId(1)).unwrap();
        assert!(!state.on_mouse(mouse(false, false)).suppress);
        state.record_drag_settlement(CaptureId(1), report).unwrap();
        assert!(!state.drag_tail_pending());
        assert!(!state.all_released()); // The pending tagged counterpart is still contained.
        assert!(!state.drag_tagged_up(1002).suppress);
        assert!(!state.all_released());
        assert!(state.end(EndReason::Aborted, at(45)).is_empty());
        assert!(state.drag_tagged_up(1001).suppress);
        assert!(state.all_released());
        assert!(!state.drag_tagged_up(1001).suppress);
    }
}

#[test]
fn send_count_alone_cannot_admit_and_wrong_nonce_or_unrelated_injection_is_inert() {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    state.mark_drag_dispatch(CaptureId(1)).unwrap();
    state
        .record_drag_settlement(CaptureId(1), DragSettlementResult::Accepted)
        .unwrap();
    assert!(!state.drag_tagged_up(0).suppress);
    assert!(!state.drag_tagged_up(1002).suppress);
    assert!(!state.on_mouse(mouse(false, true)).suppress);
    let mut buttons = [false; 256];
    buttons[0] = true;
    assert!(
        state
            .native_begin_drag(
                CaptureId(1),
                PortalId(1),
                MouseButton::PRIMARY,
                &[],
                &buttons,
                LockKeys::default(),
                at(20)
            )
            .is_err()
    );
    assert!(!state.drag_tagged_up(1001).suppress);
    assert_eq!(
        state
            .native_begin_drag(
                CaptureId(1),
                PortalId(1),
                MouseButton::PRIMARY,
                &[],
                &buttons,
                LockKeys::default(),
                at(21)
            )
            .unwrap()
            .1,
        vec![CaptureEvent::Started { id: CaptureId(1) }]
    );
    assert!(state.on_mouse(mouse(false, false)).suppress);
}

#[test]
fn neither_up_observed_uncertain_dispatch_is_pending_without_started_or_blind_resend() {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    state.mark_drag_dispatch(CaptureId(1)).unwrap();
    state
        .record_drag_settlement(CaptureId(1), DragSettlementResult::Uncertain)
        .unwrap();
    assert!(state.drag_tail_pending());
    assert!(
        state
            .record_drag_settlement(CaptureId(1), DragSettlementResult::KnownZero)
            .is_err()
    );
    assert!(state.end(EndReason::Lost, at(40)).is_empty());
    assert!(!state.drag_tagged_up(1001).suppress);
    assert!(state.on_mouse(mouse(false, false)).suppress);
    assert!(state.all_released());
}

#[test]
fn physical_first_late_tag_does_not_consume_the_next_local_primary_pair() {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    state.mark_drag_dispatch(CaptureId(1)).unwrap();
    assert!(!state.on_mouse(mouse(false, false)).suppress);
    state
        .record_drag_settlement(CaptureId(1), DragSettlementResult::Accepted)
        .unwrap();
    assert!(!state.on_mouse(mouse(true, false)).suppress);
    assert!(state.drag_tagged_up(1001).suppress);
    assert!(!state.on_mouse(mouse(false, false)).suppress);
    assert!(state.all_released());
}

fn contradictory_tag_refuses_capture(report: DragSettlementResult) {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    state.mark_drag_dispatch(CaptureId(1)).unwrap();
    assert!(!state.drag_tagged_up(1001).suppress);
    state.record_drag_settlement(CaptureId(1), report).unwrap();
    let mut buttons = [false; 256];
    buttons[0] = true;
    assert!(
        state
            .native_begin_drag(
                CaptureId(1),
                PortalId(1),
                MouseButton::PRIMARY,
                &[],
                &buttons,
                LockKeys::default(),
                at(21)
            )
            .is_err()
    );
    assert!(state.drag_tail_pending());
    assert!(state.end(EndReason::Aborted, at(22)).is_empty());
    let up = state.on_mouse(mouse(false, false));
    assert!(up.suppress);
    assert!(up.events.is_empty());
    assert!(state.all_released());
}

#[test]
fn observed_tag_does_not_promote_known_zero_into_capture() {
    contradictory_tag_refuses_capture(DragSettlementResult::KnownZero);
}

#[test]
fn observed_tag_does_not_promote_uncertain_dispatch_into_capture() {
    contradictory_tag_refuses_capture(DragSettlementResult::Uncertain);
}
