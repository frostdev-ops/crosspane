use crosspane_types::hid::{HidUsage, evdev_to_hid, hid_to_evdev};

#[test]
fn keyboard_page_has_145_mapped_usages() {
    let count = (0..=u16::MAX)
        .filter(|&id| hid_to_evdev(HidUsage::keyboard(id)).is_some())
        .count();

    assert_eq!(count, 145);
}

#[test]
fn mapped_usages_round_trip() {
    for id in 0..=u16::MAX {
        let usage = HidUsage::keyboard(id);
        if let Some(code) = hid_to_evdev(usage) {
            assert_eq!(evdev_to_hid(code), Some(usage), "usage {usage:?}");
        }
    }
}

#[test]
fn mapped_evdev_codes_round_trip() {
    for code in 0..=0x2FF {
        if let Some(usage) = evdev_to_hid(code) {
            assert_eq!(hid_to_evdev(usage), Some(code), "evdev code {code}");
        }
    }
}

#[test]
fn spot_checks() {
    for (id, code) in [
        (0x04, 30),  // KeyA, KEY_A
        (0x28, 28),  // Enter, KEY_ENTER
        (0xE1, 42),  // ShiftLeft, KEY_LEFTSHIFT
        (0xE7, 126), // MetaRight, KEY_RIGHTMETA
        (0x53, 69),  // NumLock, KEY_NUMLOCK
        (0x64, 86),  // IntlBackslash, KEY_102ND
        (0x68, 183), // F13, KEY_F13
        (0x46, 99),  // PrintScreen, KEY_SYSRQ
        (0x48, 119), // Pause, KEY_PAUSE
    ] {
        let usage = HidUsage::keyboard(id);
        assert_eq!(hid_to_evdev(usage), Some(code), "usage {usage:?}");
        assert_eq!(evdev_to_hid(code), Some(usage), "evdev code {code}");
    }
}

#[test]
fn unmapped_inputs_return_none() {
    for page in [0x00, 0x01, 0x08, 0x0C, u16::MAX] {
        for id in [0x04, 0x28, 0xE1, 0xE7] {
            let usage = HidUsage { page, id };
            assert_eq!(hid_to_evdev(usage), None, "usage {usage:?}");
        }
    }

    for id in [
        0x00,
        0x01,
        0x02,
        0x03,
        0x32,
        0x76,
        0x78,
        0x8C,
        0xA3,
        0xD8,
        u16::MAX,
    ] {
        let usage = HidUsage::keyboard(id);
        assert_eq!(hid_to_evdev(usage), None, "usage {usage:?}");
    }

    for code in [
        0, // KEY_RESERVED
        84,
        95,  // KEY_KPJPCOMMA
        101, // KEY_LINEFEED
        128, // KEY_STOP
        130, // KEY_PROPS
        139, // KEY_MENU
        142, // KEY_SLEEP (another usage page)
        0x2FF,
        u16::MAX,
    ] {
        assert_eq!(evdev_to_hid(code), None, "evdev code {code}");
    }
}
