use crosspane_types::hid::{HidUsage, hid_to_macos, macos_to_hid};

#[test]
fn keyboard_mapping_count() {
    let count = (0..=0xFFFF)
        .filter(|&id| hid_to_macos(HidUsage::keyboard(id)).is_some())
        .count();
    assert_eq!(count, 119);
}

#[test]
fn mapped_usages_round_trip() {
    for id in 0..=0xFFFF {
        let usage = HidUsage::keyboard(id);
        if let Some(kvk) = hid_to_macos(usage) {
            assert_eq!(macos_to_hid(kvk), Some(usage), "usage {id:#06x}");
        }
    }
}

#[test]
fn mapped_macos_codes_round_trip() {
    for kvk in 0..=0xFF {
        if let Some(usage) = macos_to_hid(kvk) {
            assert_eq!(hid_to_macos(usage), Some(kvk), "kVK code {kvk:#04x}");
        }
    }
}

#[test]
fn spot_checks() {
    for (id, kvk) in [
        (0x04, Some(0x00)), // KeyA, kVK_ANSI_A
        (0x28, Some(0x24)), // Enter, kVK_Return
        (0x29, Some(0x35)), // Escape, kVK_Escape
        (0xE3, Some(0x37)), // MetaLeft, kVK_Command
        (0xE7, Some(0x36)), // MetaRight, kVK_RightCommand
        (0xE2, Some(0x3A)), // AltLeft, kVK_Option
        (0x49, Some(0x72)), // Insert, kVK_Help
        (0x53, Some(0x47)), // NumLock, kVK_ANSI_KeypadClear
        (0x64, Some(0x0A)), // IntlBackslash, kVK_ISO_Section
        (0x65, Some(0x6E)), // ContextMenu, kVK_ContextualMenu
        (0x90, Some(0x68)), // Lang1, kVK_JIS_Kana
        (0x91, Some(0x66)), // Lang2, kVK_JIS_Eisu
        (0x7F, Some(0x4A)), // AudioVolumeMute, kVK_Mute
        (0x80, Some(0x48)), // AudioVolumeUp, kVK_VolumeUp
        (0x81, Some(0x49)), // AudioVolumeDown, kVK_VolumeDown
        (0x46, None),       // PrintScreen
    ] {
        assert_eq!(hid_to_macos(HidUsage::keyboard(id)), kvk, "usage {id:#06x}");
    }
}

#[test]
fn unmapped_usages_and_codes() {
    for page in [0x00, 0x01, 0x08, 0x09, 0x0C, 0xFFFF] {
        for id in [0x04, 0x7F, 0xE3] {
            assert_eq!(hid_to_macos(HidUsage { page, id }), None);
        }
    }
    for id in [
        0x00, 0x01, 0x02, 0x03, 0x32, 0x46, 0x47, 0x48, 0x66, 0x70, 0x75, 0xFFFF,
    ] {
        assert_eq!(
            hid_to_macos(HidUsage::keyboard(id)),
            None,
            "usage {id:#06x}"
        );
    }
    for kvk in [
        0x34, 0x3F, 0x42, 0x44, 0x46, 0x4D, 0x6C, 0x70, 0x7F, 0xFF, 0x100, 0xFFFF,
    ] {
        assert_eq!(macos_to_hid(kvk), None, "kVK code {kvk:#06x}");
    }
}
