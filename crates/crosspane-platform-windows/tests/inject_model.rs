#![allow(clippy::unwrap_used)]

use crosspane_platform::{error::PlatformError, session::IoGate};
use crosspane_platform_windows::model::inject::*;
use crosspane_types::{
    geom::VectorLogical,
    hid::{HidUsage, MouseButton},
    input::{LockKeys, ScrollDelta, ScrollPhase},
};
use std::collections::VecDeque;

#[derive(Default)]
struct Fake {
    foreground: VecDeque<Result<Foreground, PlatformError>>,
    sends: Vec<Packet>,
    failed: bool,
    lock_state: Option<LockKeys>,
}
impl InjectionPort for Fake {
    fn foreground(&mut self) -> Result<Foreground, PlatformError> {
        self.foreground.pop_front().unwrap_or(Ok(focus(1, 0x2000)))
    }
    fn submit(&mut self, packet: Packet) -> Result<(), PlatformError> {
        self.sends.push(packet);
        if self.failed {
            Err(PlatformError::Backend("injected failure".into()))
        } else {
            Ok(())
        }
    }
    fn locks(&mut self) -> Result<LockKeys, PlatformError> {
        Ok(self.lock_state.unwrap_or(LockKeys {
            caps_lock: Some(false),
            num_lock: Some(false),
            scroll_lock: Some(false),
        }))
    }
}
fn focus(window: u64, integrity: u32) -> Foreground {
    Foreground {
        window,
        process: 11,
        thread: 12,
        born: 13,
        generation: 0,
        integrity,
    }
}
fn driver(fake: Fake) -> Driver<Fake> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    Driver::new(fake, gate, 0x2000, RepeatSettings::new(0, 31).unwrap())
}

#[test]
fn guard_decision_table_and_releases() {
    for integrity in [0x2000, 0x1000] {
        let mut d = driver(Fake {
            foreground: [Ok(focus(1, integrity))].into(),
            ..Default::default()
        });
        d.key(HidUsage::keyboard(4), true, 0).unwrap();
        assert_eq!(d.port().sends.len(), 1);
    }
    for observation in [Ok(focus(1, 0x3000)), Err(PlatformError::SecureInput)] {
        let mut d = driver(Fake {
            foreground: [observation].into(),
            ..Default::default()
        });
        assert!(matches!(
            d.key(HidUsage::keyboard(4), true, 0),
            Err(PlatformError::SecureInput)
        ));
        assert!(d.port().sends.is_empty());
        d.gate().set_engine_permits(false);
        d.recover_keys(&[HidUsage::keyboard(4)]).unwrap();
        assert_eq!(d.port().sends.len(), 1);
    }
}

#[test]
fn focus_change_releases_both_ledgers_before_refusing_submission() {
    let mut d = driver(Fake {
        foreground: [
            Ok(focus(1, 0x2000)),
            Ok(focus(1, 0x2000)),
            Ok(focus(2, 0x2000)),
        ]
        .into(),
        ..Default::default()
    });
    d.key(HidUsage::keyboard(0xe1), true, 0).unwrap();
    d.button(MouseButton::PRIMARY, true).unwrap();
    assert!(d.submit_guarded(Packet::Move { x: 0, y: 0 }).is_err());
    assert_eq!(
        &d.port().sends[2..],
        &[
            key_packet(HidUsage::keyboard(0xe1), false).unwrap(),
            Packet::Button {
                button: MouseButton::PRIMARY,
                down: false
            }
        ]
    );
    assert!(d.held_keys().is_empty() && d.held_buttons().is_empty());
}

#[test]
fn partial_down_and_failed_release_remain_owed() {
    let mut d = driver(Fake {
        failed: true,
        ..Default::default()
    });
    assert!(d.key(HidUsage::keyboard(4), true, 0).is_err());
    assert!(d.held_keys().contains(&HidUsage::keyboard(4)));
    assert!(d.release_keys().is_err());
    assert!(d.held_keys().contains(&HidUsage::keyboard(4)));
    d.port_mut().failed = false;
    d.release_keys().unwrap();
    assert!(d.held_keys().is_empty());
}

#[test]
fn repeat_gate_epoch_and_focus_generation_cancel_without_new_ownership() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.repeat(249).unwrap();
    assert_eq!(d.port().sends.len(), 1);
    d.repeat(250).unwrap();
    assert_eq!(d.port().sends.len(), 2);
    assert_eq!(d.held_keys().len(), 1);
    d.gate().set_engine_permits(false);
    d.gate().set_engine_permits(true);
    assert!(matches!(d.repeat(1000), Err(PlatformError::Locked)));
    assert_eq!(d.port().sends.len(), 3); // owed up is allowed even after gate closure
    assert!(!d.repeating());
    assert!(d.held_keys().is_empty());
    d.key(HidUsage::keyboard(5), true, 1002).unwrap();
    d.port_mut().foreground.push_back(Ok(Foreground {
        generation: 1,
        ..focus(1, 0x2000)
    }));
    assert!(d.repeat(1300).is_err());
    assert!(d.held_keys().is_empty());
}

#[test]
fn scancode_mapping_extended_pause_and_no_unicode_fallback() {
    assert_eq!(
        key_packet(HidUsage::keyboard(4), true).unwrap(),
        Packet::Key {
            scan: 0x1e,
            extended: false,
            down: true
        }
    );
    assert_eq!(
        key_packet(HidUsage::keyboard(0xe4), false).unwrap(),
        Packet::Key {
            scan: 0x1d,
            extended: true,
            down: false
        }
    );
    assert_eq!(
        key_packet(HidUsage::keyboard(0x48), true).unwrap(),
        Packet::Key {
            scan: 0x45,
            extended: false,
            down: true
        }
    );
    assert!(key_packet(HidUsage { page: 12, id: 4 }, true).is_err());
}

#[test]
fn release_cancels_repeat_and_lock_toggle_keeps_existing_held_set() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.set_locks(LockKeys {
        caps_lock: Some(true),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(d.held_keys().len(), 1);
    d.key(HidUsage::keyboard(4), false, 1).unwrap();
    d.repeat(1000).unwrap();
    assert!(!d.repeating());
    assert!(d.held_keys().is_empty());
}

#[test]
fn repeat_settings_boundaries_are_not_guessed() {
    assert_eq!(RepeatSettings::new(0, 0).unwrap().delay_ms, 250);
    assert_eq!(RepeatSettings::new(3, 31).unwrap().delay_ms, 1000);
    assert_eq!(RepeatSettings::new(0, 0).unwrap().interval_ms, 400);
    assert!(RepeatSettings::new(4, 0).is_err());
    assert!(RepeatSettings::new(0, 32).is_err());
}

#[test]
fn move_scroll_button_and_lock_each_require_fresh_guard() {
    for operation in 0..4 {
        for observation in [Ok(focus(1, 0x3000)), Err(PlatformError::SecureInput)] {
            let mut d = driver(Fake {
                foreground: [observation].into(),
                ..Default::default()
            });
            let result = match operation {
                0 => d.submit_guarded(Packet::Move { x: 1, y: 2 }),
                1 => d.submit_guarded(Packet::Wheel {
                    horizontal: true,
                    v120: 60,
                }),
                2 => d.button(MouseButton::PRIMARY, true),
                _ => d.set_locks(LockKeys {
                    caps_lock: Some(true),
                    ..Default::default()
                }),
            };
            assert!(matches!(result, Err(PlatformError::SecureInput)));
            assert!(d.port().sends.is_empty());
        }
    }
}

#[test]
fn failed_unknown_release_and_recovered_buttons_are_owed() {
    let mut d = driver(Fake {
        failed: true,
        ..Default::default()
    });
    assert!(d.key(HidUsage::keyboard(5), false, 0).is_err());
    assert!(d.held_keys().contains(&HidUsage::keyboard(5)));
    assert!(
        d.recover_buttons(&[MouseButton::BACK, MouseButton::FORWARD])
            .is_err()
    );
    assert_eq!(d.held_buttons().len(), 2);
    d.gate().set_engine_permits(false);
    d.port_mut().failed = false;
    d.release_everything().unwrap();
    assert!(d.held_keys().is_empty() && d.held_buttons().is_empty());
}

#[test]
fn secure_refusal_keeps_failed_cleanup_owed_and_never_submits_new_down() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.port_mut().failed = true;
    d.port_mut()
        .foreground
        .push_back(Err(PlatformError::SecureInput));
    assert!(matches!(
        d.key(HidUsage::keyboard(5), true, 1),
        Err(PlatformError::SecureInput)
    ));
    assert!(d.held_keys().contains(&HidUsage::keyboard(4)));
    assert!(!d.held_keys().contains(&HidUsage::keyboard(5)));
    assert!(!d.repeating());
}

#[test]
fn lock_failure_owed_up_is_separate_from_existing_key_ownership() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.port_mut().failed = true;
    assert!(
        d.set_locks(LockKeys {
            caps_lock: Some(true),
            ..Default::default()
        })
        .is_err()
    );
    assert_eq!(
        d.held_keys().iter().copied().collect::<Vec<_>>(),
        [HidUsage::keyboard(4)]
    );
    d.port_mut().failed = false;
    d.release_keys().unwrap();
    assert!(
        d.port()
            .sends
            .contains(&key_packet(HidUsage::keyboard(0x39), false).unwrap())
    );
    d.key(HidUsage::keyboard(0x39), true, 1).unwrap();
    assert!(
        d.set_locks(LockKeys {
            caps_lock: Some(true),
            ..Default::default()
        })
        .is_err()
    );
    assert!(d.held_keys().contains(&HidUsage::keyboard(0x39)));
}

#[test]
fn repeat_last_key_no_catchup_and_every_release_or_recovery_cancels() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.key(HidUsage::keyboard(5), true, 1).unwrap();
    d.repeat(100_000).unwrap();
    assert_eq!(d.port().sends.len(), 3);
    assert_eq!(
        d.port().sends[2],
        key_packet(HidUsage::keyboard(5), true).unwrap()
    );
    d.recover_buttons(&[]).unwrap();
    d.repeat(200_000).unwrap();
    assert_eq!(d.port().sends.len(), 3);
}

#[test]
fn modifier_down_preserves_typematic_and_unmapped_up_cancels_it() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.key(HidUsage::keyboard(0xe1), true, 1).unwrap();
    d.repeat(250).unwrap();
    assert_eq!(
        d.port().sends.last(),
        Some(&key_packet(HidUsage::keyboard(4), true).unwrap())
    );
    assert!(d.key(HidUsage { page: 12, id: 4 }, false, 251).is_err());
    assert!(!d.repeating());
}

#[test]
fn unmapped_button_up_cancels_typematic_before_refusal() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    assert!(d.button(MouseButton(6), false).is_err());
    assert!(!d.repeating());
    d.repeat(1000).unwrap();
    assert_eq!(d.port().sends.len(), 1);
}

#[test]
fn pid_reuse_with_different_start_time_drains_before_new_button() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.port_mut().foreground.push_back(Ok(Foreground {
        born: 14,
        ..focus(1, 0x2000)
    }));
    assert!(d.button(MouseButton::PRIMARY, true).is_err());
    assert!(d.held_buttons().is_empty());
    assert_eq!(
        d.port().sends.last(),
        Some(&key_packet(HidUsage::keyboard(4), false).unwrap())
    );
}

fn monitor(
    path: &str,
    rect: [i32; 4],
    primary: bool,
) -> crosspane_platform_windows::model::geometry::MonitorProbe {
    crosspane_platform_windows::model::geometry::MonitorProbe {
        device_path: path.into(),
        name: path.into(),
        rc_monitor: rect,
        rc_work: rect,
        primary,
        dpi: 96,
        refresh_millihz: 60_000,
        edid: None,
        twin: false,
        quarter_turns: 0,
    }
}

#[test]
fn absolute_device_mapping_negative_origin_retained_ids_and_invalid_points() {
    use crosspane_platform_windows::model::geometry::DisplayIds;
    use crosspane_types::geom::PointDevice;
    let probes = [
        monitor("left", [-100, 0, 0, 100], false),
        monitor("primary", [0, 0, 100, 100], true),
    ];
    let mut ids = DisplayIds::default();
    let left = ids.assign("left").unwrap();
    let right = ids.assign("primary").unwrap();
    assert_eq!(
        absolute_move(&probes, &mut ids, left, PointDevice::new(0.0, 0.0))
            .unwrap()
            .0,
        Packet::Move { x: 0, y: 0 }
    );
    assert_eq!(
        absolute_move(&probes, &mut ids, right, PointDevice::new(99.0, 99.0))
            .unwrap()
            .0,
        Packet::Move { x: 65535, y: 65535 }
    );
    for point in [
        PointDevice::new(100.0, 0.0),
        PointDevice::new(-1.0, 0.0),
        PointDevice::new(f64::NAN, 1.0),
    ] {
        assert!(absolute_move(&probes, &mut ids, left, point).is_err());
    }
    assert!(absolute_move(&probes[1..], &mut ids, left, PointDevice::new(0.0, 0.0)).is_err());
    assert_eq!(ids.assign("left").unwrap(), left);
}

fn twin(path: &str, rect: [i32; 4]) -> crosspane_platform_windows::model::geometry::MonitorProbe {
    crosspane_platform_windows::model::geometry::MonitorProbe {
        twin: true,
        ..monitor(path, rect, false)
    }
}

#[test]
fn absolute_move_maps_a_twin_by_id_into_its_rect() {
    use crosspane_platform_windows::model::geometry::DisplayIds;
    use crosspane_types::geom::PointDevice;
    // Bounding box [0, 256) x [0, 128): spans 255 and 127, so 65535 / span is exact here.
    let probes = [
        monitor("primary", [0, 0, 128, 128], true),
        twin("twin", [128, 0, 256, 128]),
    ];
    let mut ids = DisplayIds::default();
    let primary = ids.assign("primary").unwrap();
    let twin_id = ids.assign("twin").unwrap();
    let (packet, probe) =
        absolute_move(&probes, &mut ids, twin_id, PointDevice::new(0.0, 0.0)).unwrap();
    assert_eq!(packet, Packet::Move { x: 32896, y: 0 });
    assert_eq!(probe, probes[1]);
    let (packet, probe) =
        absolute_move(&probes, &mut ids, twin_id, PointDevice::new(127.0, 127.0)).unwrap();
    assert_eq!(packet, Packet::Move { x: 65535, y: 65535 });
    assert_eq!(probe, probes[1]);
    assert_eq!(
        absolute_move(&probes, &mut ids, primary, PointDevice::new(127.0, 127.0))
            .unwrap()
            .0,
        Packet::Move { x: 32639, y: 65535 }
    );
}

#[test]
fn absolute_move_refuses_unknown_twin_ids_points_and_degenerate_rects() {
    use crosspane_platform_windows::model::geometry::DisplayIds;
    use crosspane_types::geom::PointDevice;
    let probes = [
        monitor("primary", [0, 0, 128, 128], true),
        twin("twin", [128, 0, 256, 128]),
        twin("flat", [256, 0, 256, 128]),
    ];
    let mut ids = DisplayIds::default();
    let twin_id = ids.assign("twin").unwrap();
    let flat = ids.assign("flat").unwrap();
    let ghost = ids.assign("ghost").unwrap();
    for (display, point) in [
        (ghost, PointDevice::new(0.0, 0.0)),
        (twin_id, PointDevice::new(128.0, 0.0)),
        (twin_id, PointDevice::new(-1.0, 0.0)),
        (flat, PointDevice::new(0.0, 0.0)),
    ] {
        assert!(matches!(
            absolute_move(&probes, &mut ids, display, point),
            Err(PlatformError::NotFound)
        ));
    }
}

#[test]
fn absolute_move_refuses_two_twin_probes_for_one_id() {
    use crosspane_platform_windows::model::geometry::DisplayIds;
    use crosspane_types::geom::PointDevice;
    let probes = [
        monitor("primary", [0, 0, 128, 128], true),
        twin("twin", [128, 0, 256, 128]),
        twin("twin", [256, 0, 384, 128]),
        twin("other", [384, 0, 512, 128]),
    ];
    let mut ids = DisplayIds::default();
    let twin_id = ids.assign("twin").unwrap();
    let other_id = ids.assign("other").unwrap();
    assert!(matches!(
        absolute_move(&probes, &mut ids, twin_id, PointDevice::new(0.0, 0.0)),
        Err(PlatformError::NotFound)
    ));
    // Another twin path does not share the ambiguity.
    assert!(absolute_move(&probes, &mut ids, other_id, PointDevice::new(0.0, 0.0)).is_ok());
    // With one of the two duplicates gone, the same id resolves.
    assert!(absolute_move(&probes[..2], &mut ids, twin_id, PointDevice::new(0.0, 0.0)).is_ok());
}

#[test]
fn fractional_wheel_units_do_not_add_pixels_or_submit_zero_stops() {
    let delta = ScrollDelta {
        v120_x: -15,
        v120_y: 60,
        pixels: Some(VectorLogical::new(-200.0, 300.0)),
        phase: ScrollPhase::Changed,
        stop_x: false,
        stop_y: false,
    };
    assert_eq!(
        scroll_packets(delta),
        [
            Packet::Wheel {
                horizontal: true,
                v120: -15
            },
            Packet::Wheel {
                horizontal: false,
                v120: 60
            }
        ]
    );
    assert!(
        scroll_packets(ScrollDelta {
            v120_x: 0,
            v120_y: 0,
            pixels: None,
            phase: ScrollPhase::Ended,
            stop_x: true,
            stop_y: true
        })
        .is_empty()
    );
}

fn pixels(x: f64, y: f64) -> ScrollDelta {
    ScrollDelta {
        v120_x: 0,
        v120_y: 0,
        pixels: Some(VectorLogical::new(x, y)),
        phase: ScrollPhase::Changed,
        stop_x: false,
        stop_y: false,
    }
}

fn wheel(horizontal: bool, v120: i32) -> Packet {
    Packet::Wheel { horizontal, v120 }
}

#[test]
fn pixel_only_sources_use_each_axis_setting_and_v120_precedence() {
    let mut d = driver(Fake::default());
    d.set_scroll_settings(ScrollSettings::new(Some(6), Some(3)));
    d.scroll(pixels(-12.0, 12.0)).unwrap();
    d.scroll(ScrollDelta {
        v120_x: -15,
        ..pixels(500.0, 10.0)
    })
    .unwrap();
    assert_eq!(
        d.port().sends,
        [
            wheel(true, -12),
            wheel(false, 24),
            wheel(true, -15),
            wheel(false, 20)
        ]
    );
    for setting in [None, Some(0), Some(u32::MAX)] {
        let mut d = driver(Fake::default());
        d.set_scroll_settings(ScrollSettings::new(setting, setting));
        d.scroll(pixels(10.0, -10.0)).unwrap();
        assert_eq!(d.port().sends, [wheel(true, 20), wheel(false, -20)]);
    }
}

#[test]
fn signed_subunits_accumulate_independently_and_sign_changes_drop_debt() {
    let mut d = driver(Fake::default());
    d.scroll(pixels(0.2, -0.3)).unwrap();
    assert!(d.port().sends.is_empty());
    d.scroll(pixels(0.2, -0.3)).unwrap();
    assert_eq!(d.port().sends, [wheel(false, -1)]);
    d.scroll(pixels(0.2, 0.0)).unwrap();
    assert_eq!(d.port().sends, [wheel(false, -1), wheel(true, 1)]);
    d.port_mut().sends.clear();
    d.reset_scroll();
    d.scroll(pixels(0.4, 0.0)).unwrap();
    d.scroll(pixels(-0.2, 0.0)).unwrap();
    d.scroll(pixels(-0.3, 0.0)).unwrap();
    assert_eq!(d.port().sends, [wheel(true, -1)]);
}

#[test]
fn lifecycle_and_per_axis_stops_reset_fractional_debt() {
    for phase in [
        ScrollPhase::Began,
        ScrollPhase::Ended,
        ScrollPhase::Cancelled,
        ScrollPhase::MomentumEnded,
    ] {
        let mut d = driver(Fake::default());
        d.scroll(pixels(0.4, 0.4)).unwrap();
        d.scroll(ScrollDelta {
            phase,
            ..pixels(0.1, 0.1)
        })
        .unwrap();
        assert!(d.port().sends.is_empty());
    }
    let mut d = driver(Fake::default());
    d.scroll(pixels(0.3, 0.3)).unwrap();
    d.scroll(ScrollDelta {
        stop_x: true,
        ..pixels(0.0, 0.0)
    })
    .unwrap();
    d.scroll(pixels(0.3, 0.3)).unwrap();
    assert_eq!(d.port().sends, [wheel(false, 1)]);
    d.port_mut().sends.clear();
    d.reset_scroll();
    for phase in [ScrollPhase::MomentumBegan, ScrollPhase::MomentumChanged] {
        d.scroll(ScrollDelta {
            phase,
            ..pixels(0.3, 0.0)
        })
        .unwrap();
    }
    assert_eq!(d.port().sends, [wheel(true, 1)]);
}

#[test]
fn nonfinite_pixels_are_rejected_and_large_emissions_clamp_without_backlog() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut d = driver(Fake::default());
        d.scroll(pixels(0.4, 0.4)).unwrap();
        assert!(
            d.scroll(ScrollDelta {
                v120_x: 1,
                ..pixels(value, 1.0)
            })
            .is_err()
        );
        d.scroll(pixels(0.1, 0.1)).unwrap();
        assert!(d.port().sends.is_empty());
    }
    let mut d = driver(Fake::default());
    d.scroll(pixels(f64::MAX, -f64::MAX)).unwrap();
    d.scroll(pixels(0.1, -0.1)).unwrap();
    assert_eq!(
        d.port().sends,
        [wheel(true, i32::MAX), wheel(false, i32::MIN)]
    );
}

#[test]
fn scroll_remainders_reset_on_generation_gate_release_and_submission_failures() {
    for reset in 0..6 {
        let mut d = driver(Fake::default());
        d.scroll(pixels(0.4, 0.0)).unwrap();
        match reset {
            0 => {
                d.release_keys().unwrap();
            }
            1 => {
                d.release_buttons().unwrap();
            }
            2 => {
                d.gate().set_engine_permits(false);
                assert!(d.scroll(pixels(0.0, 0.0)).is_err());
                d.gate().set_engine_permits(true);
            }
            3 => {
                d.gate().set_engine_permits(false);
                d.gate().set_engine_permits(true);
            }
            4 => {
                let mut changed = focus(1, 0x2000);
                changed.generation = 1;
                d.port_mut().foreground.push_back(Ok(changed));
            }
            _ => {
                d.port_mut().failed = true;
                assert!(d.scroll(pixels(0.2, 0.0)).is_err());
                d.port_mut().failed = false;
                d.port_mut().sends.clear();
            }
        }
        d.scroll(pixels(0.1, 0.0)).unwrap();
        assert!(d.port().sends.is_empty(), "reset case {reset}");
    }
}

#[test]
fn fresh_fence_between_scroll_axes_refuses_the_second_submission() {
    let mut d = driver(Fake {
        foreground: [
            Ok(focus(1, 0x2000)),
            Ok(focus(1, 0x2000)),
            Ok(focus(2, 0x2000)),
        ]
        .into(),
        ..Default::default()
    });
    assert!(matches!(
        d.scroll(pixels(10.0, 10.0)),
        Err(PlatformError::SecureInput)
    ));
    assert_eq!(d.port().sends, [wheel(true, 20)]);
    d.scroll(pixels(0.1, 0.1)).unwrap();
    assert_eq!(d.port().sends, [wheel(true, 20)]);
}

#[test]
fn unrelated_key_and_button_releases_and_equal_lock_requests_keep_pixel_fractions() {
    let mut d = driver(Fake::default());
    d.scroll(pixels(0.4, 0.0)).unwrap();
    d.key(HidUsage::keyboard(0x73), false, 0).unwrap();
    d.button(MouseButton::PRIMARY, false).unwrap();
    d.set_locks(LockKeys {
        caps_lock: Some(false),
        ..LockKeys::default()
    })
    .unwrap();
    d.port_mut().sends.clear();
    d.scroll(pixels(0.1, 0.0)).unwrap();
    assert_eq!(d.port().sends, [wheel(true, 1)]);
}

#[test]
fn unknown_lock_requests_leave_unchanged_and_known_requests_never_guess_actual_state() {
    let mut d = driver(Fake {
        lock_state: Some(LockKeys::default()),
        ..Fake::default()
    });
    assert_eq!(d.locks().unwrap(), LockKeys::default());
    d.set_locks(LockKeys::default()).unwrap();
    for wanted in [
        LockKeys {
            caps_lock: Some(true),
            ..LockKeys::default()
        },
        LockKeys {
            num_lock: Some(false),
            ..LockKeys::default()
        },
        LockKeys {
            scroll_lock: Some(true),
            ..LockKeys::default()
        },
    ] {
        assert!(matches!(
            d.set_locks(wanted),
            Err(PlatformError::SecureInput)
        ));
    }
    assert!(d.port().sends.is_empty());
}
