#![allow(clippy::unwrap_used)]

use crosspane_platform::{error::PlatformError, session::IoGate};
use crosspane_platform_windows::model::inject::*;
use crosspane_types::{
    hid::{HidUsage, MouseButton},
    input::LockKeys,
};
use std::collections::VecDeque;

#[derive(Default)]
struct Fake {
    foreground: VecDeque<Result<Foreground, PlatformError>>,
    sends: Vec<Packet>,
    failed: bool,
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
        Ok(LockKeys {
            caps_lock: Some(false),
            num_lock: Some(false),
            scroll_lock: Some(false),
        })
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

#[test]
fn fractional_wheel_units_do_not_add_pixels_and_zero_stop_is_forwarded() {
    use crosspane_types::{
        geom::VectorLogical,
        input::{ScrollDelta, ScrollPhase},
    };
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
    assert_eq!(
        scroll_packets(ScrollDelta {
            v120_x: 0,
            v120_y: 0,
            pixels: None,
            phase: ScrollPhase::Ended,
            stop_x: true,
            stop_y: true
        }),
        [Packet::Wheel {
            horizontal: false,
            v120: 0
        }]
    );
}
