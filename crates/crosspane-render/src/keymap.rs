//! Physical keyboard mapping. Consumer-page media keys have no page-0x07 equivalent.
//!
//! IDs follow USB-IF HID Usage Tables, Keyboard/Keypad page:
//! <https://www.usb.org/sites/default/files/hut1_3_0.pdf> (section 10).

use crosspane_types::hid::HidUsage;
use winit::keyboard::KeyCode;

const MAPPINGS: &[(KeyCode, u16)] = &[
    (KeyCode::KeyA, 0x04),
    (KeyCode::KeyB, 0x05),
    (KeyCode::KeyC, 0x06),
    (KeyCode::KeyD, 0x07),
    (KeyCode::KeyE, 0x08),
    (KeyCode::KeyF, 0x09),
    (KeyCode::KeyG, 0x0A),
    (KeyCode::KeyH, 0x0B),
    (KeyCode::KeyI, 0x0C),
    (KeyCode::KeyJ, 0x0D),
    (KeyCode::KeyK, 0x0E),
    (KeyCode::KeyL, 0x0F),
    (KeyCode::KeyM, 0x10),
    (KeyCode::KeyN, 0x11),
    (KeyCode::KeyO, 0x12),
    (KeyCode::KeyP, 0x13),
    (KeyCode::KeyQ, 0x14),
    (KeyCode::KeyR, 0x15),
    (KeyCode::KeyS, 0x16),
    (KeyCode::KeyT, 0x17),
    (KeyCode::KeyU, 0x18),
    (KeyCode::KeyV, 0x19),
    (KeyCode::KeyW, 0x1A),
    (KeyCode::KeyX, 0x1B),
    (KeyCode::KeyY, 0x1C),
    (KeyCode::KeyZ, 0x1D),
    (KeyCode::Digit1, 0x1E),
    (KeyCode::Digit2, 0x1F),
    (KeyCode::Digit3, 0x20),
    (KeyCode::Digit4, 0x21),
    (KeyCode::Digit5, 0x22),
    (KeyCode::Digit6, 0x23),
    (KeyCode::Digit7, 0x24),
    (KeyCode::Digit8, 0x25),
    (KeyCode::Digit9, 0x26),
    (KeyCode::Digit0, 0x27),
    (KeyCode::Enter, 0x28),
    (KeyCode::Escape, 0x29),
    (KeyCode::Backspace, 0x2A),
    (KeyCode::Tab, 0x2B),
    (KeyCode::Space, 0x2C),
    (KeyCode::Minus, 0x2D),
    (KeyCode::Equal, 0x2E),
    (KeyCode::BracketLeft, 0x2F),
    (KeyCode::BracketRight, 0x30),
    (KeyCode::Backslash, 0x31),
    (KeyCode::Semicolon, 0x33),
    (KeyCode::Quote, 0x34),
    (KeyCode::Backquote, 0x35),
    (KeyCode::Comma, 0x36),
    (KeyCode::Period, 0x37),
    (KeyCode::Slash, 0x38),
    (KeyCode::CapsLock, 0x39),
    (KeyCode::F1, 0x3A),
    (KeyCode::F2, 0x3B),
    (KeyCode::F3, 0x3C),
    (KeyCode::F4, 0x3D),
    (KeyCode::F5, 0x3E),
    (KeyCode::F6, 0x3F),
    (KeyCode::F7, 0x40),
    (KeyCode::F8, 0x41),
    (KeyCode::F9, 0x42),
    (KeyCode::F10, 0x43),
    (KeyCode::F11, 0x44),
    (KeyCode::F12, 0x45),
    (KeyCode::PrintScreen, 0x46),
    (KeyCode::ScrollLock, 0x47),
    (KeyCode::Pause, 0x48),
    (KeyCode::Insert, 0x49),
    (KeyCode::Home, 0x4A),
    (KeyCode::PageUp, 0x4B),
    (KeyCode::Delete, 0x4C),
    (KeyCode::End, 0x4D),
    (KeyCode::PageDown, 0x4E),
    (KeyCode::ArrowRight, 0x4F),
    (KeyCode::ArrowLeft, 0x50),
    (KeyCode::ArrowDown, 0x51),
    (KeyCode::ArrowUp, 0x52),
    (KeyCode::NumLock, 0x53),
    (KeyCode::NumpadDivide, 0x54),
    (KeyCode::NumpadMultiply, 0x55),
    (KeyCode::NumpadSubtract, 0x56),
    (KeyCode::NumpadAdd, 0x57),
    (KeyCode::NumpadEnter, 0x58),
    (KeyCode::Numpad1, 0x59),
    (KeyCode::Numpad2, 0x5A),
    (KeyCode::Numpad3, 0x5B),
    (KeyCode::Numpad4, 0x5C),
    (KeyCode::Numpad5, 0x5D),
    (KeyCode::Numpad6, 0x5E),
    (KeyCode::Numpad7, 0x5F),
    (KeyCode::Numpad8, 0x60),
    (KeyCode::Numpad9, 0x61),
    (KeyCode::Numpad0, 0x62),
    (KeyCode::NumpadDecimal, 0x63),
    (KeyCode::IntlBackslash, 0x64),
    (KeyCode::ContextMenu, 0x65),
    (KeyCode::Power, 0x66),
    (KeyCode::NumpadEqual, 0x67),
    (KeyCode::F13, 0x68),
    (KeyCode::F14, 0x69),
    (KeyCode::F15, 0x6A),
    (KeyCode::F16, 0x6B),
    (KeyCode::F17, 0x6C),
    (KeyCode::F18, 0x6D),
    (KeyCode::F19, 0x6E),
    (KeyCode::F20, 0x6F),
    (KeyCode::F21, 0x70),
    (KeyCode::F22, 0x71),
    (KeyCode::F23, 0x72),
    (KeyCode::F24, 0x73),
    (KeyCode::Open, 0x74),
    (KeyCode::Help, 0x75),
    (KeyCode::Select, 0x77),
    (KeyCode::Again, 0x79),
    (KeyCode::Undo, 0x7A),
    (KeyCode::Cut, 0x7B),
    (KeyCode::Copy, 0x7C),
    (KeyCode::Paste, 0x7D),
    (KeyCode::Find, 0x7E),
    (KeyCode::AudioVolumeMute, 0x7F),
    (KeyCode::AudioVolumeUp, 0x80),
    (KeyCode::AudioVolumeDown, 0x81),
    (KeyCode::NumpadComma, 0x85),
    (KeyCode::IntlRo, 0x87),
    (KeyCode::KanaMode, 0x88),
    (KeyCode::IntlYen, 0x89),
    (KeyCode::Convert, 0x8A),
    (KeyCode::NonConvert, 0x8B),
    (KeyCode::Lang1, 0x90),
    (KeyCode::Lang2, 0x91),
    (KeyCode::Lang3, 0x92),
    (KeyCode::Lang4, 0x93),
    (KeyCode::Lang5, 0x94),
    (KeyCode::NumpadParenLeft, 0xB6),
    (KeyCode::NumpadParenRight, 0xB7),
    (KeyCode::ControlLeft, 0xE0),
    (KeyCode::ShiftLeft, 0xE1),
    (KeyCode::AltLeft, 0xE2),
    (KeyCode::SuperLeft, 0xE3),
    (KeyCode::ControlRight, 0xE4),
    (KeyCode::ShiftRight, 0xE5),
    (KeyCode::AltRight, 0xE6),
    (KeyCode::SuperRight, 0xE7),
    (KeyCode::NumpadBackspace, 0xBB),
    (KeyCode::NumpadClear, 0xD8),
    (KeyCode::NumpadClearEntry, 0xD9),
    (KeyCode::NumpadHash, 0xCC),
    (KeyCode::NumpadMemoryStore, 0xD0),
    (KeyCode::NumpadMemoryRecall, 0xD1),
    (KeyCode::NumpadMemoryClear, 0xD2),
    (KeyCode::NumpadMemoryAdd, 0xD3),
    (KeyCode::NumpadMemorySubtract, 0xD4),
    (KeyCode::Abort, 0x9B),
    (KeyCode::Props, 0xA3),
    (KeyCode::Katakana, 0x92),
    (KeyCode::Hiragana, 0x93),
];

/// winit physical key → USB HID usage (page 0x07). `None` for keys without a HID equivalent.
pub fn keycode_to_hid(code: KeyCode) -> Option<HidUsage> {
    MAPPINGS
        .iter()
        .find(|(key, _)| *key == code)
        .map(|(_, id)| HidUsage::keyboard(*id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_types::hid::hid_to_evdev;
    use std::collections::BTreeMap;

    #[test]
    fn injective_except_winit_names_for_the_same_physical_key() {
        let mut usages = BTreeMap::new();
        for &(code, id) in MAPPINGS {
            if let Some(previous) = usages.insert(id, code) {
                assert!(matches!(
                    (previous, code),
                    (KeyCode::Lang3, KeyCode::Katakana) | (KeyCode::Lang4, KeyCode::Hiragana)
                ));
            }
        }
    }

    #[test]
    fn spot_checks() {
        for (code, id) in [
            (KeyCode::KeyA, 0x04),
            (KeyCode::Enter, 0x28),
            (KeyCode::ShiftLeft, 0xE1),
            (KeyCode::SuperLeft, 0xE3),
        ] {
            assert_eq!(keycode_to_hid(code), Some(HidUsage::keyboard(id)));
        }
        assert_eq!(keycode_to_hid(KeyCode::MediaPlayPause), None);
        assert_eq!(keycode_to_hid(KeyCode::F25), None);
    }

    #[test]
    fn canonical_evdev_table_and_explicit_usb_only_usages() {
        // The frozen evdev table omits these valid keyboard usages. Keep them usable on
        // platforms that can report them, and explicitly test the table's actual coverage.
        let usb_only = [
            0x9B, 0xA3, 0xBB, 0xCC, 0xD0, 0xD1, 0xD2, 0xD3, 0xD4, 0xD8, 0xD9,
        ];
        for &(code, id) in MAPPINGS {
            let usage = keycode_to_hid(code).expect("listed mapping");
            assert_eq!(usage.page, HidUsage::PAGE_KEYBOARD);
            assert_eq!(
                hid_to_evdev(usage).is_none(),
                usb_only.contains(&id),
                "{code:?}"
            );
        }
    }
}
