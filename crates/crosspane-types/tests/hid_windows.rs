use crosspane_types::hid::{HidUsage, ScanPrefix, WinScancode, hid_to_windows, windows_to_hid};

#[test]
fn keyboard_page_has_137_mapped_usages() {
    let count = (0..=u16::MAX)
        .filter(|&id| hid_to_windows(HidUsage::keyboard(id)).is_some())
        .count();

    assert_eq!(count, 137);
}

#[test]
fn every_mapped_usage_round_trips() {
    for id in 0..=u16::MAX {
        let usage = HidUsage::keyboard(id);
        if let Some(scancode) = hid_to_windows(usage) {
            assert_eq!(windows_to_hid(scancode), Some(usage), "{usage:?}");
        }
    }
}

#[test]
fn every_mapped_scancode_round_trips() {
    for prefix in [ScanPrefix::None, ScanPrefix::E0, ScanPrefix::E1] {
        for code in 0..=u8::MAX {
            let scancode = WinScancode { code, prefix };
            if let Some(usage) = windows_to_hid(scancode) {
                assert_eq!(hid_to_windows(usage), Some(scancode), "{scancode:?}");
            }
        }
    }
}

#[test]
fn known_keys_have_chromium_scancodes() {
    let cases = [
        (0x04, 0x1E, ScanPrefix::None), // KeyA
        (0x28, 0x1C, ScanPrefix::None), // Enter
        (0xE4, 0x1D, ScanPrefix::E0),   // ControlRight
        (0x49, 0x52, ScanPrefix::E0),   // Insert
        (0x53, 0x45, ScanPrefix::E0),   // NumLock
        (0x48, 0x45, ScanPrefix::None), // Pause
        (0x46, 0x37, ScanPrefix::E0),   // PrintScreen
        (0xE3, 0x5B, ScanPrefix::E0),   // MetaLeft
        (0x65, 0x5D, ScanPrefix::E0),   // ContextMenu
        (0x90, 0x72, ScanPrefix::None), // Lang1
        (0x91, 0x71, ScanPrefix::None), // Lang2
    ];

    for (id, code, prefix) in cases {
        let usage = HidUsage::keyboard(id);
        let scancode = WinScancode { code, prefix };
        assert_eq!(hid_to_windows(usage), Some(scancode), "{usage:?}");
        assert_eq!(windows_to_hid(scancode), Some(usage), "{scancode:?}");
    }
}

#[test]
fn other_pages_and_unmapped_codes_have_no_mapping() {
    for page in [0x00, 0x01, 0x09, 0x0C, u16::MAX] {
        for id in 0..=u16::MAX {
            let usage = HidUsage { page, id };
            assert_eq!(hid_to_windows(usage), None, "{usage:?}");
        }
    }

    for id in 0..0x04 {
        assert_eq!(hid_to_windows(HidUsage::keyboard(id)), None);
    }

    for code in 0..=u8::MAX {
        let scancode = WinScancode {
            code,
            prefix: ScanPrefix::E1,
        };
        assert_eq!(windows_to_hid(scancode), None, "{scancode:?}");
    }

    for prefix in [ScanPrefix::None, ScanPrefix::E0, ScanPrefix::E1] {
        let scancode = WinScancode { code: 0, prefix };
        assert_eq!(windows_to_hid(scancode), None, "{scancode:?}");
    }

    for code in [0xFC, 0xFF] {
        let scancode = WinScancode {
            code,
            prefix: ScanPrefix::None,
        };
        assert_eq!(windows_to_hid(scancode), None, "{scancode:?}");
    }
}
