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
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

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
    activate_at(state, id, 20)
}

fn activate_at(state: &mut HookState, id: u64, ms: u64) -> Vec<CaptureEvent> {
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
            at(ms),
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

fn physical_first(report: DragSettlementResult) -> HookState {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    state.mark_drag_dispatch(CaptureId(1)).unwrap();
    let mut up = mouse(false, false);
    up.at = at(40);
    assert!(!state.on_mouse(up).suppress);
    state.record_drag_settlement(CaptureId(1), report).unwrap();
    state
}

fn ordinary_at(state: &mut HookState, id: u64, ms: u64) -> Result<(), PlatformError> {
    // Model half of the real owner tick, driven only by this fake monotonic clock.
    state.expire_drag_tail(at(ms));
    state
        .native_begin(
            CaptureId(id),
            PortalId(1),
            &[],
            &[false; 256],
            LockKeys::default(),
            at(ms),
        )
        .map(|_| ())
}

#[test]
fn physical_first_uncertain_expires_exactly_at_one_second_and_ordinary_capture_resumes() {
    let mut state = physical_first(DragSettlementResult::Uncertain);
    assert!(matches!(
        ordinary_at(&mut state, 3, 1039),
        Err(PlatformError::PointerButtonHeld)
    ));
    assert!(ordinary_at(&mut state, 3, 1040).is_ok());
    assert!(state.end(EndReason::Requested, at(1041)).len() == 1);
    assert!(state.all_released());
}

#[test]
fn no_up_uncertain_expires_after_one_second_without_fabricating_settlement() {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    state.mark_drag_dispatch(CaptureId(1)).unwrap();
    state
        .record_drag_settlement(CaptureId(1), DragSettlementResult::Uncertain)
        .unwrap();
    assert!(ordinary_at(&mut state, 3, 40).is_err());
    assert!(ordinary_at(&mut state, 3, 1039).is_err());
    // This ordinary admission supplies a fresh all-up snapshot. Expiry is not UP proof.
    assert!(ordinary_at(&mut state, 3, 1040).is_ok());
    assert!(state.drag_tagged_up(1001).suppress);
    assert!(!state.drag_tagged_up(1001).suppress);
}

#[test]
fn uncertain_wait_restarts_at_the_later_physical_up_receipt() {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    state.mark_drag_dispatch(CaptureId(1)).unwrap();
    state
        .record_drag_settlement(CaptureId(1), DragSettlementResult::Uncertain)
        .unwrap();
    assert!(ordinary_at(&mut state, 3, 40).is_err());
    let mut up = mouse(false, false);
    up.at = at(900);
    assert!(!state.on_mouse(up).suppress);
    for ms in [1040, 1899] {
        assert!(matches!(
            ordinary_at(&mut state, 3, ms),
            Err(PlatformError::PointerButtonHeld)
        ));
    }
    assert!(ordinary_at(&mut state, 3, 1900).is_ok());
}

#[test]
fn expired_physical_first_swallows_only_one_matching_late_tag() {
    let mut state = physical_first(DragSettlementResult::Uncertain);
    assert!(ordinary_at(&mut state, 3, 1040).is_ok());
    assert!(state.drag_tagged_up(1001).suppress);
    assert!(!state.drag_tagged_up(1001).suppress);
    assert!(state.end(EndReason::Requested, at(1041)).len() == 1);
    assert!(state.all_released());
}

#[test]
fn expired_physical_first_wrong_tag_passes_and_preserves_matching_memory() {
    let mut state = physical_first(DragSettlementResult::Uncertain);
    assert!(ordinary_at(&mut state, 3, 1040).is_ok());
    for nonce in [0, 1002, 9999] {
        let decision = state.drag_tagged_up(nonce);
        assert!(!decision.suppress);
        assert!(decision.events.is_empty());
    }
    assert!(state.drag_tagged_up(1001).suppress);
    assert!(!state.drag_tagged_up(1001).suppress);
}

#[test]
fn injected_first_retains_real_physical_tail_indefinitely() {
    let mut state = physical_down();
    state
        .protect_drag_tail(CaptureId(1), MouseButton::PRIMARY, 1001)
        .unwrap();
    activate_at(&mut state, 1, 50);
    state.end(EndReason::Requested, at(60));
    for ms in [1040, 60_000, 1_000_000] {
        assert!(matches!(
            ordinary_at(&mut state, 3, ms),
            Err(PlatformError::PointerButtonHeld)
        ));
        assert!(state.drag_tail_pending());
    }
    let mut up = mouse(false, false);
    up.at = at(1_000_001);
    let decision = state.on_mouse(up);
    assert!(decision.suppress);
    assert!(decision.events.is_empty());
    assert!(ordinary_at(&mut state, 3, 1_000_002).is_ok());
}

#[test]
fn accepted_physical_first_matching_tag_inside_bound_keeps_existing_behavior() {
    let mut state = physical_first(DragSettlementResult::Accepted);
    assert!(matches!(
        ordinary_at(&mut state, 3, 1039),
        Err(PlatformError::PointerButtonHeld)
    ));
    assert!(state.drag_tagged_up(1001).suppress);
    assert!(ordinary_at(&mut state, 3, 1040).is_ok());
    assert!(!state.drag_tagged_up(1001).suppress);
}

#[test]
fn new_drag_replaces_expired_nonce_memory() {
    let mut state = physical_first(DragSettlementResult::Accepted);
    assert!(ordinary_at(&mut state, 3, 1040).is_ok());
    state.end(EndReason::Requested, at(1041));
    let mut down = mouse(true, false);
    down.at = at(1042);
    assert!(!state.on_mouse(down).suppress);
    state
        .protect_drag_tail(CaptureId(2), MouseButton::PRIMARY, 1002)
        .unwrap();
    assert!(!state.drag_tagged_up(1001).suppress);
    assert_eq!(
        activate_at(&mut state, 2, 1060),
        vec![CaptureEvent::Started { id: CaptureId(2) }]
    );
    let mut up = mouse(false, false);
    up.at = at(1061);
    let up = state.on_mouse(up);
    assert!(up.suppress);
    assert_eq!(up.events.len(), 1);
}

#[test]
fn expired_old_tail_does_not_pass_a_new_ordinary_captures_physical_up() {
    let mut state = physical_first(DragSettlementResult::Uncertain);
    assert!(ordinary_at(&mut state, 3, 1040).is_ok());
    let mut down = mouse(true, false);
    down.at = at(1041);
    let down = state.on_mouse(down);
    assert!(down.suppress);
    assert_eq!(down.events.len(), 1);
    let mut up = mouse(false, false);
    up.at = at(1042);
    let up = state.on_mouse(up);
    assert!(up.suppress);
    assert_eq!(
        up.events,
        vec![CaptureEvent::Button {
            button: MouseButton::PRIMARY,
            down: false,
            at: at(1042),
        }]
    );
    assert!(state.drag_tagged_up(1001).suppress);
    assert!(state.end(EndReason::Requested, at(1043)).len() == 1);
    assert!(state.all_released());
}

// These are the exact atomic transfer/late-tag helpers delegated to by native Shared and
// the tagged-UP branch. No Win32 hook is installed and no fake input reaches an OS.
struct NativeTail {
    order: AtomicU8,
    first: AtomicU8,
    nonce: AtomicUsize,
    late: AtomicUsize,
    original: AtomicU64,
    primary: AtomicU8,
    token: AtomicU64,
}

impl NativeTail {
    fn new(order: u8) -> Self {
        Self {
            order: AtomicU8::new(order),
            first: AtomicU8::new(if order == 4 { 4 } else { 0 }),
            nonce: AtomicUsize::new(1001),
            late: AtomicUsize::new(0),
            original: AtomicU64::new(401),
            primary: AtomicU8::new(if order == 4 { 0 } else { 2 }),
            token: AtomicU64::new(401),
        }
    }
    fn expire(&self, nonce: usize) -> bool {
        hook::expire_drag_shared(
            nonce,
            &self.order,
            &self.first,
            &self.nonce,
            &self.late,
            &self.original,
            &self.primary,
            &self.token,
        )
    }
}

#[test]
fn native_expiry_clears_old_order_then_current_capture_suppresses_its_physical_up() {
    let tail = NativeTail::new(4);
    assert!(tail.expire(1001));
    assert_eq!(tail.order.load(Ordering::Acquire), 0);
    assert_eq!(tail.first.load(Ordering::Acquire), 0);
    assert_eq!(tail.nonce.load(Ordering::Acquire), 0);
    assert_eq!(tail.token.load(Ordering::Acquire), 0);
    tail.primary.store(2, Ordering::Release);
    tail.token.store(801, Ordering::Release);
    // With native drag_order zero, the existing ordinary LL branch uses this frozen table.
    assert_eq!(
        crosspane_platform_windows::model::capture::key_transition(
            tail.primary.load(Ordering::Acquire),
            true,
            true,
        ),
        (0, true)
    );
    assert!(hook::consume_late_drag_tag(&tail.late, 1001));
    assert!(!hook::consume_late_drag_tag(&tail.late, 1001));
    assert_eq!(tail.token.load(Ordering::Acquire), 801);
}

#[test]
fn native_uncertain_expiry_preserves_unreleased_local_down_and_never_posts_up() {
    let tail = NativeTail::new(5);
    assert!(tail.expire(1001));
    assert_eq!(tail.primary.load(Ordering::Acquire), 1);
    assert_eq!(tail.token.load(Ordering::Acquire), 0);
    assert_eq!(tail.late.load(Ordering::Acquire), 1001);
    assert_eq!(
        crosspane_platform_windows::model::capture::key_transition(
            tail.primary.load(Ordering::Acquire),
            false,
            true,
        ),
        (0, false)
    );
}

#[test]
fn native_expiry_cannot_clear_newer_reservation_or_injected_first() {
    for order in [2, 3, 4, 5] {
        let tail = NativeTail::new(order);
        if order != 3 {
            tail.nonce.store(1002, Ordering::Release);
        }
        assert!(!tail.expire(1001));
        assert_eq!(tail.order.load(Ordering::Acquire), order);
        assert_eq!(
            tail.nonce.load(Ordering::Acquire),
            if order == 3 { 1001 } else { 1002 }
        );
        assert_eq!(tail.token.load(Ordering::Acquire), 401);
        assert_eq!(tail.late.load(Ordering::Acquire), 0);
    }
}

#[test]
fn native_expiry_preserves_a_newer_current_capture_token_and_primary_state() {
    let tail = NativeTail::new(4);
    tail.token.store(801, Ordering::Release);
    tail.primary.store(2, Ordering::Release);
    assert!(tail.expire(1001));
    assert_eq!(tail.token.load(Ordering::Acquire), 801);
    assert_eq!(tail.primary.load(Ordering::Acquire), 2);
    assert_eq!(tail.order.load(Ordering::Acquire), 0);
}

#[test]
fn native_late_tag_is_exact_once_and_new_memory_replaces_old_nonce() {
    let tail = NativeTail::new(4);
    assert!(tail.expire(1001));
    for nonce in [0, 1002, 9999] {
        assert!(!hook::consume_late_drag_tag(&tail.late, nonce));
        assert_eq!(tail.late.load(Ordering::Acquire), 1001);
    }
    assert!(hook::consume_late_drag_tag(&tail.late, 1001));
    assert_eq!(tail.late.load(Ordering::Acquire), 0);
    assert!(!hook::consume_late_drag_tag(&tail.late, 1001));
    tail.late.store(1002, Ordering::Release);
    assert!(!hook::consume_late_drag_tag(&tail.late, 1001));
    assert!(hook::consume_late_drag_tag(&tail.late, 1002));
}
