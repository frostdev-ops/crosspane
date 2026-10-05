#![allow(clippy::unwrap_used)]

use crosspane_platform::{Chord, HotkeyEvent, PlatformError};
use crosspane_platform_windows::model::hotkey::{
    HotkeyModel, RawKey, registration_available, registration_owned,
};
use crosspane_types::{hid::HidUsage, time::MonoTime};

const NOW: MonoTime = MonoTime::from_nanos(100);
fn chord() -> Chord {
    Chord {
        modifiers: vec![HidUsage::keyboard(0xe0)],
        key: HidUsage::keyboard(0x29),
    }
}
fn raw(model: &mut HotkeyModel, device: usize, code: u16, flags: u16) -> Option<HotkeyEvent> {
    model.raw(
        RawKey {
            device,
            make_code: code,
            flags,
        },
        NOW,
    )
}
fn configured() -> HotkeyModel {
    let mut model = HotkeyModel::default();
    model.configure(&chord(), &[], NOW).unwrap();
    model
}

#[test]
fn hotkey_initial_state_is_released_and_subscription_is_once() {
    let mut model = configured();
    assert_eq!(
        model.subscribe(NOW).unwrap(),
        HotkeyEvent::Released { at: NOW }
    );
    assert!(model.subscribe(NOW).is_err());
}
#[test]
fn hotkey_held_seed_reports_initial_pressed_without_duplicate_repeat() {
    let mut model = HotkeyModel::default();
    let chord = chord();
    assert_eq!(
        model
            .configure(&chord, &[chord.modifiers[0], chord.key], NOW)
            .unwrap(),
        vec![HotkeyEvent::Pressed { at: NOW }]
    );
    assert_eq!(
        model.subscribe(NOW).unwrap(),
        HotkeyEvent::Pressed { at: NOW }
    );
    assert_eq!(raw(&mut model, 1, 0x1d, 0), None);
    assert_eq!(raw(&mut model, 1, 1, 0), None);
    assert_eq!(
        raw(&mut model, 1, 1, 1),
        Some(HotkeyEvent::Released { at: NOW })
    );
}
#[test]
fn hotkey_pair_repeats_and_capture_boundaries_do_not_reset() {
    let mut model = configured();
    assert_eq!(raw(&mut model, 1, 0x1d, 0), None);
    assert_eq!(
        raw(&mut model, 1, 1, 0),
        Some(HotkeyEvent::Pressed { at: NOW })
    );
    for _ in 0..4 {
        assert!(model.configure(&chord(), &[], NOW).unwrap().is_empty());
        assert_eq!(raw(&mut model, 1, 1, 0), None);
    }
    assert_eq!(
        raw(&mut model, 1, 1, 1),
        Some(HotkeyEvent::Released { at: NOW })
    );
    assert_eq!(raw(&mut model, 1, 1, 1), None);
}
#[test]
fn hotkey_null_device_reports_do_not_change_held_state() {
    let mut model = configured();
    raw(&mut model, 1, 0x1d, 0);
    assert_eq!(raw(&mut model, 0, 1, 0), None);
    assert_eq!(
        raw(&mut model, 1, 1, 0),
        Some(HotkeyEvent::Pressed { at: NOW })
    );
    assert_eq!(raw(&mut model, 0, 1, 1), None);
    assert_eq!(
        raw(&mut model, 1, 1, 1),
        Some(HotkeyEvent::Released { at: NOW })
    );
}
#[test]
fn hotkey_malformed_reports_are_inert() {
    let mut model = configured();
    raw(&mut model, 1, 0x1d, 0);
    for (code, flags) in [(1, 8), (1, 6), (0, 0), (0xff, 0), (0x101, 0)] {
        assert_eq!(raw(&mut model, 1, code, flags), None);
    }
    assert_eq!(
        raw(&mut model, 1, 1, 0),
        Some(HotkeyEvent::Pressed { at: NOW })
    );
}
#[test]
fn hotkey_prefixes_preserve_right_control_identity() {
    let mut model = HotkeyModel::default();
    model
        .configure(
            &Chord {
                modifiers: vec![HidUsage::keyboard(0xe4)],
                key: chord().key,
            },
            &[],
            NOW,
        )
        .unwrap();
    raw(&mut model, 1, 0x1d, 0);
    assert_eq!(raw(&mut model, 1, 1, 0), None);
    raw(&mut model, 1, 1, 1);
    raw(&mut model, 1, 0x1d, 2);
    assert_eq!(
        raw(&mut model, 1, 1, 0),
        Some(HotkeyEvent::Pressed { at: NOW })
    );
}
#[test]
fn hotkey_multiple_devices_do_not_release_another_devices_key() {
    let mut model = configured();
    raw(&mut model, 1, 0x1d, 0);
    raw(&mut model, 2, 0x1d, 0);
    raw(&mut model, 1, 1, 0);
    assert_eq!(raw(&mut model, 2, 0x1d, 1), None);
    assert_eq!(
        raw(&mut model, 1, 0x1d, 1),
        Some(HotkeyEvent::Released { at: NOW })
    );
}
#[test]
fn hotkey_main_key_release_is_required_to_rearm() {
    let mut model = configured();
    raw(&mut model, 1, 0x1d, 0);
    raw(&mut model, 1, 1, 0);
    assert!(matches!(
        raw(&mut model, 1, 0x1d, 1),
        Some(HotkeyEvent::Released { .. })
    ));
    raw(&mut model, 1, 0x1d, 0);
    assert_eq!(raw(&mut model, 1, 1, 0), None);
    raw(&mut model, 1, 1, 1);
    assert!(matches!(
        raw(&mut model, 1, 1, 0),
        Some(HotkeyEvent::Pressed { .. })
    ));
}
#[test]
fn hotkey_replacement_settles_old_pair_and_unsupported_is_unchanged() {
    let mut model = configured();
    raw(&mut model, 1, 0x1d, 0);
    raw(&mut model, 1, 1, 0);
    assert!(matches!(
        model.configure(
            &Chord {
                modifiers: vec![],
                key: HidUsage::keyboard(0xffff)
            },
            &[],
            NOW
        ),
        Err(PlatformError::Unsupported(_))
    ));
    assert_eq!(model.chord(), Some(&chord()));
    let new = Chord {
        modifiers: vec![],
        key: HidUsage::keyboard(0x04),
    };
    assert_eq!(
        model.configure(&new, &[], NOW).unwrap(),
        vec![HotkeyEvent::Released { at: NOW }]
    );
}
#[test]
fn hotkey_source_loss_is_terminal_without_fabricated_release() {
    let mut model = configured();
    raw(&mut model, 1, 0x1d, 0);
    raw(&mut model, 1, 1, 0);
    model.lose();
    assert_eq!(raw(&mut model, 1, 1, 1), None);
    assert!(model.available().is_err());
    assert!(model.configure(&chord(), &[], NOW).is_err());
    assert!(model.subscribe(NOW).is_err());
}
#[test]
fn hotkey_registration_refuses_existing_and_detects_replacement() {
    assert!(registration_available(&[]));
    assert!(!registration_available(&[0]));
    assert!(!registration_available(&[7]));
    assert!(registration_owned(&[7], 7));
    for inventory in [&[][..], &[0][..], &[8][..], &[7, 8][..]] {
        assert!(!registration_owned(inventory, 7));
    }
}

#[test]
fn hotkey_subscribe_seeds_fresh_state_and_never_resets_established_pair() {
    let mut model = configured();
    let chord = chord();
    model
        .seed_initial(&[chord.modifiers[0], chord.key], NOW)
        .unwrap();
    assert_eq!(
        model.subscribe(NOW).unwrap(),
        HotkeyEvent::Pressed { at: NOW }
    );
    assert!(model.seed_initial(&[], NOW).is_err());
    assert_eq!(
        raw(&mut model, 1, 1, 1),
        Some(HotkeyEvent::Released { at: NOW })
    );
}

#[test]
fn hotkey_subscribe_snapshot_can_observe_release_before_subscription() {
    let mut model = configured();
    raw(&mut model, 1, 0x1d, 0);
    raw(&mut model, 1, 1, 0);
    model.seed_initial(&[], NOW).unwrap();
    assert_eq!(
        model.subscribe(NOW).unwrap(),
        HotkeyEvent::Released { at: NOW }
    );
}
