use crosspane_input::Held;
use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, Chord, Edge, EndReason, HotkeyEvent, MotionKind,
    PlatformError, PortalId,
};
use crosspane_platform_windows::model::hook::*;
use crosspane_types::geom::{
    DisplayGeometry, PixelRect, PixelSize, PointLogical, SizeMm, euclid::point2,
};
use crosspane_types::hid::{
    HidUsage, MouseButton, ScanPrefix, WinScancode, hid_to_windows, windows_to_hid,
};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollPhase};
use crosspane_types::time::MonoTime;
use proptest::prelude::*;

fn at(value: u64) -> MonoTime {
    MonoTime::from_nanos(value * 1_000_000)
}

fn portal(id: u32, x: i32) -> (CapturePortal, PixelRect) {
    (
        CapturePortal {
            id: PortalId(id),
            display: DisplayId(7),
            edge: Edge::Right,
            from: 150.0,
            to: 450.0,
        },
        PixelRect::new(point2(x, 150), point2(x + 1, 450)),
    )
}

#[allow(clippy::unwrap_used)] // Invalid injected setup must fail the test.
fn state() -> HookState {
    let mut state = HookState::new();
    assert!(
        state
            .set_portals(vec![portal(1, 1499)], at(0))
            .unwrap()
            .is_empty()
    );
    state
}

#[allow(clippy::unwrap_used)] // Invalid injected activation must fail the test.
fn begin(state: &mut HookState, id: u64, held: &[(u8, bool)]) {
    let held: Vec<_> = held
        .iter()
        .map(|&(code, extended)| (code, extended, 0x41))
        .collect();
    let (_, events) = state
        .begin(
            CaptureId(id),
            PortalId(1),
            &held,
            &[false; 256],
            LockKeys::default(),
            at(10),
        )
        .unwrap();
    assert_eq!(
        events.last(),
        Some(&CaptureEvent::Started { id: CaptureId(id) })
    );
}

fn key(code: u8, extended: bool, up: bool) -> KeyIn {
    KeyIn {
        scancode: code,
        extended,
        vk: 0x41,
        up,
        injected: false,
        ours: false,
        at: at(20),
    }
}

fn mouse(kind: MouseKind, pt: (i32, i32)) -> MouseIn {
    MouseIn {
        kind,
        pt,
        delta: None,
        dragged: false,
        buttons_down: None,
        injected: false,
        ours: false,
        at: at(20),
    }
}

fn moved(pt: (i32, i32)) -> MouseIn {
    let mut input = mouse(MouseKind::Move, pt);
    input.delta = Some((1.0, 0.0)); // Existing right-edge fixtures attest outward motion.
    input
}

fn button(n: u8, down: bool) -> MouseIn {
    mouse(MouseKind::Button { n, down }, (1499, 300))
}

fn pressed(id: u32) -> CaptureEvent {
    CaptureEvent::EdgePressed {
        portal: PortalId(id),
        position: 0.5,
        at: at(20),
    }
}

fn released(id: u32) -> CaptureEvent {
    CaptureEvent::EdgeReleased {
        portal: PortalId(id),
        at: at(20),
    }
}

#[test]
fn windows_numeric_constants_match_primary_documentation() {
    assert_eq!(
        (LLKHF_EXTENDED, LLKHF_LOWER_IL_INJECTED, LLKHF_INJECTED),
        (1, 2, 0x10)
    );
    assert_eq!((LLKHF_ALTDOWN, LLKHF_UP), (0x20, 0x80));
    assert_eq!((LLMHF_INJECTED, LLMHF_LOWER_IL_INJECTED), (1, 2));
    assert_eq!(
        (
            WHEEL_DELTA,
            XBUTTON1,
            XBUTTON2,
            LOW_LEVEL_HOOKS_TIMEOUT_MAX_MS
        ),
        (120, 1, 2, 1000)
    );
}

#[test]
fn key_usage_reuses_every_frozen_scan_slot_and_literal_table_quirks() {
    for code in 0..=255 {
        for extended in [false, true] {
            let scan = WinScancode {
                code,
                prefix: if extended {
                    ScanPrefix::E0
                } else {
                    ScanPrefix::None
                },
            };
            assert_eq!(key_usage(code, extended), windows_to_hid(scan));
        }
    }
    for (code, extended, usage) in [(0x45, false, 0x48), (0x45, true, 0x53), (0x37, true, 0x46)] {
        assert_eq!(key_usage(code, extended), Some(HidUsage::keyboard(usage)));
    }
    assert_eq!(key_usage(0, false), None);
}

#[test]
fn activation_reports_literal_held_keys_locks_and_started_after_edge_release() {
    let mut state = state();
    assert_eq!(state.on_mouse(moved((1499, 300))).events, vec![pressed(1)]);
    let locks = LockKeys {
        caps_lock: Some(true),
        num_lock: Some(false),
        scroll_lock: None,
    };
    let (start, events) = state
        .begin(
            CaptureId(0),
            PortalId(1),
            &[
                (0x1e, false, 0x41),
                (0x1e, false, 0x41),
                (0x1d, true, 0x41),
                (0, false, 0x41),
            ],
            &[false; 256],
            locks,
            at(21),
        )
        .unwrap();
    assert_eq!(
        start.held_keys,
        vec![HidUsage::keyboard(4), HidUsage::keyboard(0xe4)]
    );
    assert_eq!(start.lock_keys, locks);
    assert_eq!(
        events,
        vec![
            CaptureEvent::EdgeReleased {
                portal: PortalId(1),
                at: at(21)
            },
            CaptureEvent::Started { id: CaptureId(0) },
        ]
    );
    assert!(!state.all_released());
}

#[test]
fn activation_held_local_key_repeats_are_silent_and_its_up_reaches_local_os() {
    let mut state = state();
    begin(&mut state, 1, &[(0x1e, false)]);
    for _ in 0..4 {
        assert_eq!(
            state.on_key(key(0x1e, false, false)),
            Decision {
                suppress: true,
                events: vec![]
            }
        );
    }
    assert_eq!(
        state.on_key(key(0x1e, false, true)),
        Decision {
            suppress: false,
            events: vec![CaptureEvent::Key {
                usage: HidUsage::keyboard(4),
                down: false,
                at: at(20)
            }],
        }
    );
    assert_eq!(
        state.on_key(key(0x1e, false, false)),
        Decision {
            suppress: true,
            events: vec![CaptureEvent::Key {
                usage: HidUsage::keyboard(4),
                down: true,
                at: at(20)
            }],
        }
    );
}

#[test]
fn captured_key_repeat_tail_end_and_new_local_press_matrix() {
    for reason in [EndReason::Requested, EndReason::Lost, EndReason::Aborted] {
        let mut state = state();
        begin(&mut state, 1, &[]);
        let down = state.on_key(key(0x1e, false, false));
        assert_eq!(
            down,
            Decision {
                suppress: true,
                events: vec![CaptureEvent::Key {
                    usage: HidUsage::keyboard(4),
                    down: true,
                    at: at(20)
                }],
            }
        );
        assert_eq!(
            state.on_key(key(0x1e, false, false)),
            Decision {
                suppress: true,
                events: vec![]
            }
        );
        assert_eq!(
            state.end(reason, at(21)),
            vec![CaptureEvent::Ended {
                id: CaptureId(1),
                reason
            }]
        );
        assert!(state.end(reason, at(22)).is_empty());
        assert!(!state.all_released());
        assert_eq!(
            state.on_key(key(0x1e, false, false)),
            Decision {
                suppress: true,
                events: vec![]
            }
        );
        assert_eq!(
            state.on_key(key(0x1e, false, true)),
            Decision {
                suppress: true,
                events: vec![]
            }
        );
        assert!(state.all_released());
        assert_eq!(state.on_key(key(0x1e, false, false)), Decision::default());
        assert_eq!(state.on_key(key(0x1e, false, true)), Decision::default());
    }
}

#[test]
fn old_captured_key_up_clears_new_activation_held_chord_without_replaying_down() {
    let mut state = state();
    begin(&mut state, 1, &[]);
    state.on_key(key(0x1e, false, false));
    state.end(EndReason::Requested, at(21));
    let (start, events) = state
        .begin(
            CaptureId(2),
            PortalId(1),
            &[(0x1e, false, 0x41)],
            &[false; 256],
            LockKeys::default(),
            at(22),
        )
        .unwrap();
    assert_eq!(start.held_keys, vec![HidUsage::keyboard(4)]);
    assert_eq!(events, vec![CaptureEvent::Started { id: CaptureId(2) }]);
    assert_eq!(
        state.on_key(key(0x1e, false, false)),
        Decision {
            suppress: true,
            events: vec![]
        }
    );
    assert_eq!(
        state.on_key(key(0x1e, false, true)),
        Decision {
            suppress: true,
            events: vec![CaptureEvent::Key {
                usage: HidUsage::keyboard(4),
                down: false,
                at: at(20)
            }],
        }
    );
}

#[test]
fn unmapped_keys_are_suppressed_without_virtual_key_fallback_in_both_prefix_slots() {
    let mut state = state();
    begin(&mut state, 1, &[]);
    for extended in [false, true] {
        for code in [0, 0xff] {
            let mut input = key(code, extended, false);
            input.vk = 0x41;
            assert_eq!(key_usage(code, extended), None);
            assert_eq!(
                state.on_key(input),
                Decision {
                    suppress: true,
                    events: vec![]
                }
            );
            input.up = true;
            assert_eq!(
                state.on_key(input),
                Decision {
                    suppress: true,
                    events: vec![]
                }
            );
        }
    }
}

#[test]
fn injected_or_ours_input_passes_without_changing_capture_or_physical_tails() {
    for ours in [false, true] {
        let mut state = state();
        assert_eq!(state.on_mouse(moved((1499, 300))).events, vec![pressed(1)]);
        for kind in [
            MouseKind::Move,
            MouseKind::Button { n: 0, down: true },
            MouseKind::Wheel {
                v120: 120,
                horizontal: false,
            },
        ] {
            let mut input = mouse(kind, (1499, 300));
            input.ours = ours;
            input.injected = !ours;
            assert_eq!(state.on_mouse(input), Decision::default());
        }
        begin(&mut state, 1, &[]);
        assert!(state.on_key(key(0x1e, false, false)).suppress);
        assert!(state.on_mouse(button(0, true)).suppress);
        let mut input = key(0x1e, false, true);
        input.ours = ours;
        input.injected = !ours;
        assert_eq!(state.on_key(input), Decision::default());
        let mut input = button(0, false);
        input.ours = ours;
        input.injected = !ours;
        assert_eq!(state.on_mouse(input), Decision::default());
        state.end(EndReason::Lost, at(21));
        assert!(!state.all_released());
        assert!(state.on_key(key(0x1e, false, true)).suppress);
        assert!(!state.all_released());
        assert!(state.on_mouse(button(0, false)).suppress);
        assert!(state.all_released());
    }
}

#[test]
fn held_and_suppressed_buttons_cover_every_raw_number_and_do_not_overflow() {
    for n in 0..=255 {
        let mut state = state();
        state.on_mouse(button(n, true));
        let mut held = [false; 256];
        held[usize::from(n)] = true;
        assert!(matches!(
            state.begin(
                CaptureId(1),
                PortalId(1),
                &[],
                &held,
                LockKeys::default(),
                at(10),
            ),
            Err(PlatformError::PointerButtonHeld)
        ));
        assert_eq!(state.on_mouse(button(n, false)), Decision::default());
        begin(&mut state, 1, &[]);
        let down = state.on_mouse(button(n, true));
        assert!(down.suppress);
        let expected = n
            .checked_add(1)
            .map(MouseButton)
            .map(|button| CaptureEvent::Button {
                button,
                down: true,
                at: at(20),
            })
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(down.events, expected);
        assert_eq!(
            state.on_mouse(button(n, true)),
            Decision {
                suppress: true,
                events: vec![]
            }
        );
        state.end(EndReason::Requested, at(21));
        assert!(!state.all_released());
        assert_eq!(
            state.on_mouse(button(n, false)),
            Decision {
                suppress: true,
                events: vec![]
            }
        );
        assert!(state.all_released());
        assert_eq!(state.on_mouse(button(n, true)), Decision::default());
    }
}

#[test]
fn old_button_up_is_suppressed_but_reported_only_for_its_capture_token() {
    let mut state = state();
    begin(&mut state, 1, &[]);
    assert_eq!(
        state.on_mouse(button(0, true)).events,
        vec![CaptureEvent::Button {
            button: MouseButton::PRIMARY,
            down: true,
            at: at(20)
        },]
    );
    state.end(EndReason::Requested, at(21));
    // Fresh OS snapshot is already up, but the old physical callback is still buffered.
    begin(&mut state, 2, &[]);
    assert_eq!(
        state.on_mouse(button(0, false)),
        Decision {
            suppress: true,
            events: vec![]
        }
    );
    assert_eq!(
        state.on_mouse(button(0, true)).events,
        vec![CaptureEvent::Button {
            button: MouseButton::PRIMARY,
            down: true,
            at: at(20)
        },]
    );
    assert_eq!(
        state.on_mouse(button(0, false)).events,
        vec![CaptureEvent::Button {
            button: MouseButton::PRIMARY,
            down: false,
            at: at(20)
        },]
    );
}

#[test]
fn drag_across_physical_strip_never_presses_for_any_tracked_button() {
    for n in [0, 1, 2, 3, 4, 7, 63, 64, 127, 128, 255] {
        let mut state = state();
        assert_eq!(state.on_mouse(button(n, true)), Decision::default());
        for _ in 0..5 {
            let mut input = moved((1499, 300));
            input.dragged = true;
            assert_eq!(state.on_mouse(input), Decision::default());
        }
        assert_eq!(state.on_mouse(button(n, false)), Decision::default());
        assert_eq!(state.on_mouse(moved((1499, 300))).events, vec![pressed(1)]);
    }
}

#[test]
fn missed_down_drag_observation_blocks_press_even_when_snapshot_is_already_up() {
    let mut state = state();
    let mut input = moved((1499, 300));
    input.dragged = true;
    input.buttons_down = Some([false; 8]);
    assert_eq!(state.on_mouse(input), Decision::default());
}

#[test]
fn down_releases_pressed_edge_immediately_up_does_not_press_until_later_motion() {
    for n in [0, 1, 2, 3, 4, 7, 255] {
        let mut state = state();
        assert_eq!(state.on_mouse(moved((1499, 300))).events, vec![pressed(1)]);
        assert_eq!(state.on_mouse(button(n, true)).events, vec![released(1)]);
        let mut input = moved((1499, 300));
        input.dragged = true;
        assert_eq!(state.on_mouse(input), Decision::default());
        assert_eq!(state.on_mouse(button(n, false)), Decision::default());
        assert_eq!(state.on_mouse(moved((1499, 300))).events, vec![pressed(1)]);
    }
}

#[test]
fn last_button_up_not_first_restores_edge_eligibility() {
    let mut state = state();
    state.on_mouse(button(0, true));
    state.on_mouse(button(1, true));
    state.on_mouse(button(0, false));
    let mut input = moved((1499, 300));
    let mut snapshot = [false; 8];
    snapshot[1] = true;
    input.buttons_down = Some(snapshot);
    assert_eq!(state.on_mouse(input), Decision::default());
    state.on_mouse(button(1, false));
    assert_eq!(state.on_mouse(moved((1499, 300))).events, vec![pressed(1)]);
}

#[test]
fn missed_button_up_is_reconciled_from_literal_motion_snapshot() {
    for n in 0..8 {
        let mut state = state();
        state.on_mouse(button(n, true));
        let mut input = moved((1499, 300));
        let mut snapshot = [false; 8];
        snapshot[usize::from(n)] = true;
        input.buttons_down = Some(snapshot);
        assert_eq!(state.on_mouse(input), Decision::default());
        input.buttons_down = Some([false; 8]);
        assert_eq!(state.on_mouse(input).events, vec![pressed(1)]);
        assert_eq!(state.on_mouse(moved((1499, 300))).events, vec![pressed(1)]);
    }
}

#[test]
fn missed_down_still_held_snapshot_releases_already_pressed_portal() {
    let mut state = state();
    state.on_mouse(moved((1499, 300)));
    let mut input = moved((1499, 300));
    let mut snapshot = [false; 8];
    snapshot[0] = true;
    input.buttons_down = Some(snapshot);
    assert_eq!(state.on_mouse(input).events, vec![released(1)]);
}

#[test]
fn plain_motion_keeps_enter_stay_swap_leave_and_deterministic_edge_order() {
    let mut state = state();
    state
        .set_portals(vec![portal(2, 1500), portal(1, 1499)], at(0))
        .unwrap();
    assert_eq!(state.on_mouse(moved((1499, 300))).events, vec![pressed(1)]);
    assert_eq!(state.on_mouse(moved((1499, 300))).events, vec![pressed(1)]);
    assert_eq!(
        state.on_mouse(moved((1500, 300))).events,
        vec![released(1), pressed(2)]
    );
    assert_eq!(state.on_mouse(moved((1501, 300))).events, vec![released(2)]);
}

#[test]
fn physical_strip_at_fractional_scale_is_not_scaled_twice_and_motion_is_device_pixels() {
    let geometry = DisplayGeometry {
        physical_size: SizeMm::new(600.0, 300.0),
        pixel_size: PixelSize::new(3000, 1500),
        scale: 1.5,
        logical_origin: PointLogical::new(-1000.0, 0.0),
    };
    let physical_y = geometry
        .logical_to_device(PointLogical::new(-1000.0, 200.0))
        .y;
    assert_eq!(physical_y, 300.0);
    let mut state = state();
    assert_eq!(
        state.on_mouse(moved((1499, physical_y as i32))).events,
        vec![pressed(1)]
    );
    assert_eq!(state.on_mouse(moved((1498, 300))).events, vec![released(1)]);
    state.on_mouse(moved((1499, 300)));
    begin(&mut state, 1, &[]);
    assert_eq!(
        state.on_mouse(mouse(MouseKind::Move, (1496, 306))),
        Decision {
            suppress: true,
            events: vec![CaptureEvent::Motion {
                dx: -3.0,
                dy: 6.0,
                kind: MotionKind::Accelerated {
                    display: DisplayId(7)
                },
                at: at(20),
            }],
        }
    );
    let mut injected = moved((i32::MAX, i32::MIN));
    injected.injected = true;
    assert_eq!(state.on_mouse(injected), Decision::default());
    assert!(
        matches!(state.on_mouse(mouse(MouseKind::Move, (1497, 307))).events.as_slice(),
            [CaptureEvent::Motion { dx, dy, .. }] if *dx == 1.0 && *dy == 1.0
        )
    );
}

#[test]
fn captured_wheel_preserves_sign_partial_detents_axis_and_v120_extremes() {
    let mut state = state();
    for horizontal in [false, true] {
        for v120 in [i32::MIN, -240, -120, -1, 0, 1, 120, 240, i32::MAX] {
            let input = mouse(MouseKind::Wheel { v120, horizontal }, (0, 0));
            assert_eq!(state.on_mouse(input), Decision::default());
        }
    }
    begin(&mut state, 1, &[]);
    for horizontal in [false, true] {
        for v120 in [i32::MIN, -240, -120, -1, 0, 1, 120, 240, i32::MAX] {
            let decision = state.on_mouse(mouse(MouseKind::Wheel { v120, horizontal }, (0, 0)));
            assert!(decision.suppress);
            assert!(
                matches!(decision.events.as_slice(), [CaptureEvent::Scroll { delta, at: time }]
                    if delta.v120_x == if horizontal { v120 } else { 0 }
                    && delta.v120_y == if horizontal { 0 } else { v120 }
                    && delta.pixels.is_none() && delta.phase == ScrollPhase::Discrete
                    && !delta.stop_x && !delta.stop_y && *time == at(20)
                )
            );
        }
    }
}

#[test]
fn reconcile_after_blindness_clears_only_observed_releases_and_never_active_tails() {
    let mut state = state();
    begin(&mut state, 1, &[]);
    state.on_key(key(0x1e, false, false));
    state.on_mouse(button(255, true));
    state.reconcile(&[], &[false; 256]);
    state.end(EndReason::Lost, at(21));
    assert!(!state.all_released());
    let mut keys = vec![(0x1e, false, 0x41)];
    let mut buttons = [false; 256];
    buttons[255] = true;
    state.reconcile(&keys, &buttons);
    assert!(!state.all_released());
    keys.clear();
    state.reconcile(&keys, &buttons);
    assert!(!state.all_released());
    buttons[255] = false;
    state.reconcile(&keys, &buttons);
    assert!(state.all_released());
    assert_eq!(state.on_key(key(0x1e, false, false)), Decision::default());
    assert_eq!(state.on_mouse(button(255, true)), Decision::default());
}

#[test]
fn begin_refusals_and_invalid_portal_replacement_preserve_previous_state() {
    let mut state = state();
    let mut held = [false; 256];
    held[4] = true;
    assert!(matches!(
        state.begin(
            CaptureId(1),
            PortalId(1),
            &[],
            &held,
            LockKeys::default(),
            at(10),
        ),
        Err(PlatformError::PointerButtonHeld)
    ));
    assert!(matches!(
        state.begin(
            CaptureId(1),
            PortalId(99),
            &[],
            &[false; 256],
            LockKeys::default(),
            at(10),
        ),
        Err(PlatformError::NotFound)
    ));
    assert!(state.all_released());
    state.on_mouse(moved((1499, 300)));
    for invalid in 0..5 {
        let mut item = portal(1, 1499);
        match invalid {
            0 => item.0.from = f64::NAN,
            1 => item.0.to = item.0.from,
            2 => item.0.from = -1.0,
            3 => item.1.max = item.1.min,
            _ => item.0.to = f64::INFINITY,
        }
        assert!(state.set_portals(vec![item], at(21)).is_err());
        assert_eq!(state.on_mouse(moved((1499, 300))).events, vec![pressed(1)]);
    }
    assert!(
        state
            .set_portals(vec![portal(1, 0), portal(1, 10)], at(21))
            .is_err()
    );
    assert_eq!(state.on_mouse(moved((1499, 300))).events, vec![pressed(1)]);
    assert_eq!(
        state.set_portals(vec![], at(21)).unwrap(),
        vec![CaptureEvent::EdgeReleased {
            portal: PortalId(1),
            at: at(21)
        },]
    );
    state.set_portals(vec![portal(1, 1499)], at(22)).unwrap();
    begin(&mut state, 1, &[]);
    state.on_key(key(0x1e, false, false));
    assert!(
        state
            .begin(
                CaptureId(2),
                PortalId(1),
                &[],
                &[false; 256],
                LockKeys::default(),
                at(23),
            )
            .is_err()
    );
    assert_eq!(
        state.end(EndReason::Requested, at(24)),
        vec![CaptureEvent::Ended {
            id: CaptureId(1),
            reason: EndReason::Requested
        },]
    );
    assert!(state.on_key(key(0x1e, false, true)).suppress);
}

fn chord() -> Chord {
    Chord {
        modifiers: vec![HidUsage::keyboard(0xe0), HidUsage::keyboard(0xe2)],
        key: HidUsage::keyboard(0x29),
    }
}

#[test]
fn chord_tracker_emits_one_pair_requires_fresh_key_press_and_ignores_injected() {
    let chord = chord();
    let mut tracker = ChordTracker::default();
    assert!(tracker.set_chord(&chord, at(0)).is_empty());
    for modifier in &chord.modifiers {
        assert_eq!(tracker.on_key(*modifier, true, false, at(1)), None);
    }
    assert_eq!(tracker.on_key(chord.key, true, true, at(2)), None);
    assert_eq!(
        tracker.on_key(chord.key, true, false, at(3)),
        Some(HotkeyEvent::Pressed { at: at(3) })
    );
    assert_eq!(tracker.on_key(chord.key, true, false, at(4)), None);
    assert_eq!(tracker.on_key(chord.key, false, true, at(5)), None);
    assert_eq!(
        tracker.on_key(HidUsage::keyboard(4), true, false, at(6)),
        None
    );
    assert_eq!(
        tracker.on_key(chord.modifiers[0], false, false, at(7)),
        Some(HotkeyEvent::Released { at: at(7) })
    );
    assert_eq!(tracker.on_key(chord.modifiers[0], true, false, at(8)), None);
    assert_eq!(tracker.on_key(chord.key, true, false, at(9)), None);
    assert_eq!(tracker.on_key(chord.key, false, false, at(10)), None);
    assert_eq!(
        tracker.on_key(chord.key, true, false, at(11)),
        Some(HotkeyEvent::Pressed { at: at(11) })
    );
    assert_eq!(
        tracker.on_key(chord.key, false, false, at(12)),
        Some(HotkeyEvent::Released { at: at(12) })
    );
    assert_eq!(tracker.on_key(chord.key, false, false, at(13)), None);
}

#[test]
fn chord_replacement_is_idempotent_and_reports_already_held_initial_state() {
    let chord = chord();
    let mut tracker = ChordTracker::default();
    for usage in chord.modifiers.iter().chain([&chord.key]) {
        assert_eq!(tracker.on_key(*usage, true, false, at(1)), None);
    }
    assert_eq!(
        tracker.set_chord(&chord, at(2)),
        vec![HotkeyEvent::Pressed { at: at(2) }]
    );
    assert!(tracker.set_chord(&chord, at(3)).is_empty());
    let mut replacement = chord.clone();
    replacement.key = HidUsage::keyboard(4);
    assert_eq!(
        tracker.set_chord(&replacement, at(4)),
        vec![HotkeyEvent::Released { at: at(4) }]
    );
    assert_eq!(tracker.on_key(chord.key, false, false, at(5)), None);
    assert_eq!(
        tracker.on_key(replacement.key, true, false, at(6)),
        Some(HotkeyEvent::Pressed { at: at(6) })
    );
    assert_eq!(
        tracker.on_key(replacement.key, false, false, at(7)),
        Some(HotkeyEvent::Released { at: at(7) })
    );
}

#[test]
fn capture_start_and_end_cannot_reset_or_duplicate_independent_chord_observations() {
    let chord = Chord {
        modifiers: vec![HidUsage::keyboard(0xe0)],
        key: HidUsage::keyboard(4),
    };
    let mut tracker = ChordTracker::default();
    tracker.set_chord(&chord, at(0));
    tracker.on_key(chord.modifiers[0], true, false, at(1));
    assert_eq!(
        tracker.on_key(chord.key, true, false, at(2)),
        Some(HotkeyEvent::Pressed { at: at(2) })
    );
    let mut state = state();
    begin(&mut state, 1, &[(0x1e, false), (0x1d, false)]);
    assert_eq!(tracker.on_key(chord.key, true, false, at(3)), None);
    state.end(EndReason::Requested, at(4));
    assert_eq!(
        tracker.on_key(chord.key, false, false, at(5)),
        Some(HotkeyEvent::Released { at: at(5) })
    );
}

#[test]
fn release_plan_round_trips_every_frozen_table_entry_and_five_native_buttons() {
    let mut count = 0;
    for id in 0..=u16::MAX {
        let usage = HidUsage::keyboard(id);
        if let Some(scan) = hid_to_windows(usage) {
            let plan = release_plan(&[Held::Key(usage)]).unwrap();
            assert_eq!(
                plan,
                vec![InputRecord::Key {
                    scancode: scan,
                    up: true
                }]
            );
            assert_eq!(windows_to_hid(scan), Some(usage));
            count += 1;
        }
    }
    assert!(count > 100);
    for n in 1..=5 {
        let held = Held::Button(MouseButton(n));
        assert_eq!(
            release_plan(&[held]).unwrap(),
            vec![InputRecord::MouseUp {
                button: MouseButton(n)
            }]
        );
    }
    assert!(release_plan(&[]).unwrap().is_empty());
}

#[test]
fn release_plan_deduplicates_and_refuses_partial_unknown_recovery() {
    let a = Held::Key(HidUsage::keyboard(4));
    assert_eq!(release_plan(&[a, a]).unwrap().len(), 1);
    for unknown in [
        Held::Key(HidUsage {
            page: 0xffff,
            id: 4,
        }),
        Held::Key(HidUsage::keyboard(0xffff)),
        Held::Button(MouseButton(0)),
        Held::Button(MouseButton(6)),
        Held::Button(MouseButton(255)),
    ] {
        assert!(release_plan(&[a, unknown]).is_err());
    }
}

#[test]
fn distinct_zero_scan_keys_keep_independent_suppressed_release_tails() {
    for extended in [false, true] {
        for first in [0x41, 0x42] {
            let mut state = state();
            begin(&mut state, 1, &[]);
            for vk in [0x41, 0x42] {
                let mut input = key(0, extended, false);
                input.vk = vk;
                assert_eq!(
                    state.on_key(input),
                    Decision {
                        suppress: true,
                        events: vec![],
                    }
                );
            }
            state.end(EndReason::Requested, at(21));
            for (index, vk) in [first, if first == 0x41 { 0x42 } else { 0x41 }]
                .into_iter()
                .enumerate()
            {
                let mut input = key(0, extended, true);
                input.vk = vk;
                assert_eq!(
                    state.on_key(input),
                    Decision {
                        suppress: true,
                        events: vec![],
                    }
                );
                assert_eq!(state.all_released(), index == 1);
            }
        }
    }
}

#[test]
fn zero_scan_activation_snapshots_preserve_local_and_previous_capture_identities() {
    for captured_before in [false, true] {
        let mut state = state();
        if captured_before {
            begin(&mut state, 1, &[]);
            for vk in [0x41, 0x42] {
                state.on_key(KeyIn {
                    vk,
                    ..key(0, false, false)
                });
            }
            state.end(EndReason::Requested, at(21));
        }
        let (start, _) = state
            .begin(
                CaptureId(2),
                PortalId(1),
                &[(0, false, 0x41), (0, false, 0x42)],
                &[false; 256],
                LockKeys::default(),
                at(22),
            )
            .unwrap();
        assert!(start.held_keys.is_empty());
        for vk in [0x41, 0x42] {
            assert_eq!(
                state.on_key(KeyIn {
                    vk,
                    ..key(0, false, false)
                }),
                Decision {
                    suppress: true,
                    events: vec![]
                }
            );
            assert_eq!(
                state.on_key(KeyIn {
                    vk,
                    ..key(0, false, true)
                }),
                Decision {
                    suppress: captured_before,
                    events: vec![]
                }
            );
        }
        state.end(EndReason::Requested, at(23));
        assert!(state.all_released());
    }
}

#[test]
fn zero_scan_reconciliation_clears_only_the_absent_identity() {
    let mut state = state();
    begin(&mut state, 1, &[]);
    for vk in [0x41, 0x42] {
        state.on_key(KeyIn {
            vk,
            ..key(0, false, false)
        });
    }
    state.end(EndReason::Lost, at(21));
    state.reconcile(&[(0, false, 0x42)], &[false; 256]);
    assert!(!state.all_released());
    assert_eq!(
        state.on_key(KeyIn {
            vk: 0x41,
            ..key(0, false, true)
        }),
        Decision::default()
    );
    assert_eq!(
        state.on_key(KeyIn {
            vk: 0x42,
            ..key(0, false, true)
        }),
        Decision {
            suppress: true,
            events: vec![]
        }
    );
    assert!(state.all_released());
}

#[test]
fn duplicate_key_ups_never_repeat_captured_or_activation_held_release() {
    for locally_held in [false, true] {
        let mut state = state();
        begin(
            &mut state,
            1,
            if locally_held { &[(0x1e, false)] } else { &[] },
        );
        if !locally_held {
            assert_eq!(state.on_key(key(0x1e, false, false)).events.len(), 1);
        }
        assert_eq!(state.on_key(key(0x1e, false, true)).events.len(), 1);
        assert_eq!(
            state.on_key(key(0x1e, false, true)),
            Decision {
                suppress: true,
                events: vec![],
            }
        );
        assert!(state.on_key(key(0x30, false, true)).events.is_empty());
    }
}

#[test]
fn duplicate_button_ups_never_repeat_token_bound_release() {
    for n in 0..=255 {
        let mut state = state();
        begin(&mut state, 1, &[]);
        let mapped = usize::from(n != 255);
        assert_eq!(state.on_mouse(button(n, true)).events.len(), mapped);
        assert_eq!(state.on_mouse(button(n, false)).events.len(), mapped);
        assert_eq!(
            state.on_mouse(button(n, false)),
            Decision {
                suppress: true,
                events: vec![],
            }
        );
        assert!(state.on_mouse(button(n, false)).events.is_empty());
    }
}

#[allow(clippy::unwrap_used)] // Invalid injected portal must fail the test.
fn pressure_matrix(edge: Edge) {
    let (point, rect, outward, parallel) = match edge {
        Edge::Left => (
            (100, 500),
            PixelRect::new(point2(100, 200), point2(101, 800)),
            (-1, 0),
            (0, 1),
        ),
        Edge::Right => (
            (999, 500),
            PixelRect::new(point2(999, 200), point2(1000, 800)),
            (1, 0),
            (0, 1),
        ),
        Edge::Top => (
            (500, 200),
            PixelRect::new(point2(200, 200), point2(800, 201)),
            (0, -1),
            (1, 0),
        ),
        Edge::Bottom => (
            (500, 799),
            PixelRect::new(point2(200, 799), point2(800, 800)),
            (0, 1),
            (1, 0),
        ),
    };
    for (clamped, delta) in [false, true].into_iter().flat_map(|clamped| {
        [parallel, (-outward.0, -outward.1), (0, 0), outward].map(|delta| (clamped, delta))
    }) {
        let mut state = HookState::new();
        state
            .set_portals(
                vec![(
                    CapturePortal {
                        id: PortalId(1),
                        display: DisplayId(7),
                        edge,
                        from: 200.0,
                        to: 800.0,
                    },
                    rect,
                )],
                at(0),
            )
            .unwrap();
        state.on_mouse(mouse(
            MouseKind::Wheel {
                v120: 0,
                horizontal: false,
            },
            if clamped {
                point
            } else {
                (point.0 - delta.0, point.1 - delta.1)
            },
        ));
        let mut input = mouse(MouseKind::Move, point);
        input.delta = clamped.then_some((f64::from(delta.0), f64::from(delta.1)));
        assert_eq!(
            state.on_mouse(input).events,
            if delta == outward {
                vec![pressed(1)]
            } else {
                vec![]
            },
            "{edge:?}: {delta:?}, clamped={clamped}"
        );
        input.delta = Some((f64::from(outward.0), f64::from(outward.1)));
        assert_eq!(state.on_mouse(input).events, vec![pressed(1)]);
        input.delta = Some((f64::from(parallel.0), f64::from(parallel.1)));
        assert_eq!(state.on_mouse(input).events, vec![released(1)]);
    }
}

#[test]
fn left_portal_requires_outward_motion_not_strip_occupancy() {
    pressure_matrix(Edge::Left);
}

#[test]
fn right_portal_requires_outward_motion_not_strip_occupancy() {
    pressure_matrix(Edge::Right);
}

#[test]
fn top_portal_requires_outward_motion_not_strip_occupancy() {
    pressure_matrix(Edge::Top);
}

#[test]
fn bottom_portal_requires_outward_motion_not_strip_occupancy() {
    pressure_matrix(Edge::Bottom);
}

#[test]
fn unavailable_or_nonfinite_delta_does_not_create_pressure_or_captured_motion() {
    let mut state = state();
    assert_eq!(
        state.on_mouse(mouse(MouseKind::Move, (1499, 300))),
        Decision::default()
    );
    for delta in [(f64::NAN, 0.0), (f64::INFINITY, 0.0), (1.0, f64::INFINITY)] {
        let mut input = moved((1499, 300));
        input.delta = Some(delta);
        assert_eq!(state.on_mouse(input), Decision::default());
        begin(&mut state, 1, &[]);
        assert_eq!(
            state.on_mouse(input),
            Decision {
                suppress: true,
                events: vec![]
            }
        );
        state.end(EndReason::Requested, at(21));
    }
    begin(&mut state, 2, &[]);
    let mut input = moved((1499, 300));
    input.delta = Some((3.0, -1.0));
    assert!(matches!(
        state.on_mouse(input).events.as_slice(),
        [CaptureEvent::Motion {
            dx: 3.0,
            dy: -1.0,
            ..
        }]
    ));
}

proptest! {
    #[test]
    fn two_distinct_zero_scan_keys_never_share_suppressed_tails(
        a in 1u32..=254, b in 1u32..=254, extended in any::<bool>(),
    ) {
        prop_assume!(a != b);
        let mut state = state();
        begin(&mut state, 1, &[]);
        for vk in [a, b] {
            prop_assert!(state.on_key(KeyIn { vk, ..key(0, extended, false) }).suppress,
                "zero-scan down must be suppressed");
        }
        state.end(EndReason::Requested, at(21));
        prop_assert!(state.on_key(KeyIn { vk: a, ..key(0, extended, true) }).suppress,
            "first zero-scan up must be suppressed");
        prop_assert!(!state.all_released());
        prop_assert!(state.on_key(KeyIn { vk: b, ..key(0, extended, true) }).suppress,
            "second zero-scan up must be suppressed");
        prop_assert!(state.all_released());
    }

    #[test]
    fn every_fresh_suppressed_key_down_keeps_its_matching_up_suppressed(
        code in any::<u8>(), extended in any::<bool>(), repeats in 0usize..8,
        restart in any::<bool>(), reason in 0usize..3,
    ) {
        let mut state = state();
        begin(&mut state, 1, &[]);
        prop_assert!(state.on_key(key(code, extended, false)).suppress);
        for _ in 0..repeats {
            let repeat = state.on_key(key(code, extended, false));
            prop_assert!(repeat.suppress);
            prop_assert!(repeat.events.is_empty());
            let mut injected = key(code, extended, true);
            injected.injected = true;
            prop_assert_eq!(state.on_key(injected), Decision::default());
        }
        state.end([EndReason::Requested, EndReason::Lost, EndReason::Aborted][reason], at(21));
        if restart {
            begin(&mut state, 2, &[(code, extended)]);
        }
        prop_assert!(state.on_key(key(code, extended, true)).suppress);
        state.end(EndReason::Requested, at(22));
        prop_assert!(state.all_released());
    }

    #[test]
    fn every_suppressed_button_down_keeps_matching_up_suppressed_across_capture_boundaries(
        n in any::<u8>(), repeats in 0usize..8, restart in any::<bool>(),
    ) {
        let mut state = state();
        begin(&mut state, 1, &[]);
        prop_assert!(state.on_mouse(button(n, true)).suppress);
        for _ in 0..repeats {
            let repeat = state.on_mouse(button(n, true));
            prop_assert!(repeat.suppress);
            prop_assert!(repeat.events.is_empty());
        }
        state.end(EndReason::Lost, at(21));
        if restart {
            begin(&mut state, 2, &[]);
        }
        let up = state.on_mouse(button(n, false));
        prop_assert!(up.suppress);
        prop_assert!(up.events.is_empty());
        state.end(EndReason::Requested, at(22));
        prop_assert!(state.all_released());
    }
}
