//! HID usage ↔ Windows scan code (set 1, with E0/E1 prefixes). The table is filled in by WP-0.5.

use serde::{Deserialize, Serialize};

use super::HidUsage;

/// The prefix byte of a set-1 scan code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScanPrefix {
    None,
    /// Extended keys (`KEYEVENTF_EXTENDEDKEY`).
    E0,
    /// Only Pause/Break.
    E1,
}

/// A Windows set-1 scan code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WinScancode {
    pub code: u8,
    pub prefix: ScanPrefix,
}

/// (HID usage, scan code) pairs.
const TABLE: &[(HidUsage, WinScancode)] = &[
    (
        HidUsage::keyboard(0x04),
        WinScancode {
            code: 0x1E,
            prefix: ScanPrefix::None,
        },
    ), // KeyA
    (
        HidUsage::keyboard(0x05),
        WinScancode {
            code: 0x30,
            prefix: ScanPrefix::None,
        },
    ), // KeyB
    (
        HidUsage::keyboard(0x06),
        WinScancode {
            code: 0x2E,
            prefix: ScanPrefix::None,
        },
    ), // KeyC
    (
        HidUsage::keyboard(0x07),
        WinScancode {
            code: 0x20,
            prefix: ScanPrefix::None,
        },
    ), // KeyD
    (
        HidUsage::keyboard(0x08),
        WinScancode {
            code: 0x12,
            prefix: ScanPrefix::None,
        },
    ), // KeyE
    (
        HidUsage::keyboard(0x09),
        WinScancode {
            code: 0x21,
            prefix: ScanPrefix::None,
        },
    ), // KeyF
    (
        HidUsage::keyboard(0x0A),
        WinScancode {
            code: 0x22,
            prefix: ScanPrefix::None,
        },
    ), // KeyG
    (
        HidUsage::keyboard(0x0B),
        WinScancode {
            code: 0x23,
            prefix: ScanPrefix::None,
        },
    ), // KeyH
    (
        HidUsage::keyboard(0x0C),
        WinScancode {
            code: 0x17,
            prefix: ScanPrefix::None,
        },
    ), // KeyI
    (
        HidUsage::keyboard(0x0D),
        WinScancode {
            code: 0x24,
            prefix: ScanPrefix::None,
        },
    ), // KeyJ
    (
        HidUsage::keyboard(0x0E),
        WinScancode {
            code: 0x25,
            prefix: ScanPrefix::None,
        },
    ), // KeyK
    (
        HidUsage::keyboard(0x0F),
        WinScancode {
            code: 0x26,
            prefix: ScanPrefix::None,
        },
    ), // KeyL
    (
        HidUsage::keyboard(0x10),
        WinScancode {
            code: 0x32,
            prefix: ScanPrefix::None,
        },
    ), // KeyM
    (
        HidUsage::keyboard(0x11),
        WinScancode {
            code: 0x31,
            prefix: ScanPrefix::None,
        },
    ), // KeyN
    (
        HidUsage::keyboard(0x12),
        WinScancode {
            code: 0x18,
            prefix: ScanPrefix::None,
        },
    ), // KeyO
    (
        HidUsage::keyboard(0x13),
        WinScancode {
            code: 0x19,
            prefix: ScanPrefix::None,
        },
    ), // KeyP
    (
        HidUsage::keyboard(0x14),
        WinScancode {
            code: 0x10,
            prefix: ScanPrefix::None,
        },
    ), // KeyQ
    (
        HidUsage::keyboard(0x15),
        WinScancode {
            code: 0x13,
            prefix: ScanPrefix::None,
        },
    ), // KeyR
    (
        HidUsage::keyboard(0x16),
        WinScancode {
            code: 0x1F,
            prefix: ScanPrefix::None,
        },
    ), // KeyS
    (
        HidUsage::keyboard(0x17),
        WinScancode {
            code: 0x14,
            prefix: ScanPrefix::None,
        },
    ), // KeyT
    (
        HidUsage::keyboard(0x18),
        WinScancode {
            code: 0x16,
            prefix: ScanPrefix::None,
        },
    ), // KeyU
    (
        HidUsage::keyboard(0x19),
        WinScancode {
            code: 0x2F,
            prefix: ScanPrefix::None,
        },
    ), // KeyV
    (
        HidUsage::keyboard(0x1A),
        WinScancode {
            code: 0x11,
            prefix: ScanPrefix::None,
        },
    ), // KeyW
    (
        HidUsage::keyboard(0x1B),
        WinScancode {
            code: 0x2D,
            prefix: ScanPrefix::None,
        },
    ), // KeyX
    (
        HidUsage::keyboard(0x1C),
        WinScancode {
            code: 0x15,
            prefix: ScanPrefix::None,
        },
    ), // KeyY
    (
        HidUsage::keyboard(0x1D),
        WinScancode {
            code: 0x2C,
            prefix: ScanPrefix::None,
        },
    ), // KeyZ
    (
        HidUsage::keyboard(0x1E),
        WinScancode {
            code: 0x02,
            prefix: ScanPrefix::None,
        },
    ), // Digit1
    (
        HidUsage::keyboard(0x1F),
        WinScancode {
            code: 0x03,
            prefix: ScanPrefix::None,
        },
    ), // Digit2
    (
        HidUsage::keyboard(0x20),
        WinScancode {
            code: 0x04,
            prefix: ScanPrefix::None,
        },
    ), // Digit3
    (
        HidUsage::keyboard(0x21),
        WinScancode {
            code: 0x05,
            prefix: ScanPrefix::None,
        },
    ), // Digit4
    (
        HidUsage::keyboard(0x22),
        WinScancode {
            code: 0x06,
            prefix: ScanPrefix::None,
        },
    ), // Digit5
    (
        HidUsage::keyboard(0x23),
        WinScancode {
            code: 0x07,
            prefix: ScanPrefix::None,
        },
    ), // Digit6
    (
        HidUsage::keyboard(0x24),
        WinScancode {
            code: 0x08,
            prefix: ScanPrefix::None,
        },
    ), // Digit7
    (
        HidUsage::keyboard(0x25),
        WinScancode {
            code: 0x09,
            prefix: ScanPrefix::None,
        },
    ), // Digit8
    (
        HidUsage::keyboard(0x26),
        WinScancode {
            code: 0x0A,
            prefix: ScanPrefix::None,
        },
    ), // Digit9
    (
        HidUsage::keyboard(0x27),
        WinScancode {
            code: 0x0B,
            prefix: ScanPrefix::None,
        },
    ), // Digit0
    (
        HidUsage::keyboard(0x28),
        WinScancode {
            code: 0x1C,
            prefix: ScanPrefix::None,
        },
    ), // Enter
    (
        HidUsage::keyboard(0x29),
        WinScancode {
            code: 0x01,
            prefix: ScanPrefix::None,
        },
    ), // Escape
    (
        HidUsage::keyboard(0x2A),
        WinScancode {
            code: 0x0E,
            prefix: ScanPrefix::None,
        },
    ), // Backspace
    (
        HidUsage::keyboard(0x2B),
        WinScancode {
            code: 0x0F,
            prefix: ScanPrefix::None,
        },
    ), // Tab
    (
        HidUsage::keyboard(0x2C),
        WinScancode {
            code: 0x39,
            prefix: ScanPrefix::None,
        },
    ), // Space
    (
        HidUsage::keyboard(0x2D),
        WinScancode {
            code: 0x0C,
            prefix: ScanPrefix::None,
        },
    ), // Minus
    (
        HidUsage::keyboard(0x2E),
        WinScancode {
            code: 0x0D,
            prefix: ScanPrefix::None,
        },
    ), // Equal
    (
        HidUsage::keyboard(0x2F),
        WinScancode {
            code: 0x1A,
            prefix: ScanPrefix::None,
        },
    ), // BracketLeft
    (
        HidUsage::keyboard(0x30),
        WinScancode {
            code: 0x1B,
            prefix: ScanPrefix::None,
        },
    ), // BracketRight
    (
        HidUsage::keyboard(0x31),
        WinScancode {
            code: 0x2B,
            prefix: ScanPrefix::None,
        },
    ), // Backslash
    (
        HidUsage::keyboard(0x33),
        WinScancode {
            code: 0x27,
            prefix: ScanPrefix::None,
        },
    ), // Semicolon
    (
        HidUsage::keyboard(0x34),
        WinScancode {
            code: 0x28,
            prefix: ScanPrefix::None,
        },
    ), // Quote
    (
        HidUsage::keyboard(0x35),
        WinScancode {
            code: 0x29,
            prefix: ScanPrefix::None,
        },
    ), // Backquote
    (
        HidUsage::keyboard(0x36),
        WinScancode {
            code: 0x33,
            prefix: ScanPrefix::None,
        },
    ), // Comma
    (
        HidUsage::keyboard(0x37),
        WinScancode {
            code: 0x34,
            prefix: ScanPrefix::None,
        },
    ), // Period
    (
        HidUsage::keyboard(0x38),
        WinScancode {
            code: 0x35,
            prefix: ScanPrefix::None,
        },
    ), // Slash
    (
        HidUsage::keyboard(0x39),
        WinScancode {
            code: 0x3A,
            prefix: ScanPrefix::None,
        },
    ), // CapsLock
    (
        HidUsage::keyboard(0x3A),
        WinScancode {
            code: 0x3B,
            prefix: ScanPrefix::None,
        },
    ), // F1
    (
        HidUsage::keyboard(0x3B),
        WinScancode {
            code: 0x3C,
            prefix: ScanPrefix::None,
        },
    ), // F2
    (
        HidUsage::keyboard(0x3C),
        WinScancode {
            code: 0x3D,
            prefix: ScanPrefix::None,
        },
    ), // F3
    (
        HidUsage::keyboard(0x3D),
        WinScancode {
            code: 0x3E,
            prefix: ScanPrefix::None,
        },
    ), // F4
    (
        HidUsage::keyboard(0x3E),
        WinScancode {
            code: 0x3F,
            prefix: ScanPrefix::None,
        },
    ), // F5
    (
        HidUsage::keyboard(0x3F),
        WinScancode {
            code: 0x40,
            prefix: ScanPrefix::None,
        },
    ), // F6
    (
        HidUsage::keyboard(0x40),
        WinScancode {
            code: 0x41,
            prefix: ScanPrefix::None,
        },
    ), // F7
    (
        HidUsage::keyboard(0x41),
        WinScancode {
            code: 0x42,
            prefix: ScanPrefix::None,
        },
    ), // F8
    (
        HidUsage::keyboard(0x42),
        WinScancode {
            code: 0x43,
            prefix: ScanPrefix::None,
        },
    ), // F9
    (
        HidUsage::keyboard(0x43),
        WinScancode {
            code: 0x44,
            prefix: ScanPrefix::None,
        },
    ), // F10
    (
        HidUsage::keyboard(0x44),
        WinScancode {
            code: 0x57,
            prefix: ScanPrefix::None,
        },
    ), // F11
    (
        HidUsage::keyboard(0x45),
        WinScancode {
            code: 0x58,
            prefix: ScanPrefix::None,
        },
    ), // F12
    (
        HidUsage::keyboard(0x46),
        WinScancode {
            code: 0x37,
            prefix: ScanPrefix::E0,
        },
    ), // PrintScreen
    (
        HidUsage::keyboard(0x47),
        WinScancode {
            code: 0x46,
            prefix: ScanPrefix::None,
        },
    ), // ScrollLock
    (
        HidUsage::keyboard(0x48),
        WinScancode {
            code: 0x45,
            prefix: ScanPrefix::None,
        },
    ), // Pause
    (
        HidUsage::keyboard(0x49),
        WinScancode {
            code: 0x52,
            prefix: ScanPrefix::E0,
        },
    ), // Insert
    (
        HidUsage::keyboard(0x4A),
        WinScancode {
            code: 0x47,
            prefix: ScanPrefix::E0,
        },
    ), // Home
    (
        HidUsage::keyboard(0x4B),
        WinScancode {
            code: 0x49,
            prefix: ScanPrefix::E0,
        },
    ), // PageUp
    (
        HidUsage::keyboard(0x4C),
        WinScancode {
            code: 0x53,
            prefix: ScanPrefix::E0,
        },
    ), // Delete
    (
        HidUsage::keyboard(0x4D),
        WinScancode {
            code: 0x4F,
            prefix: ScanPrefix::E0,
        },
    ), // End
    (
        HidUsage::keyboard(0x4E),
        WinScancode {
            code: 0x51,
            prefix: ScanPrefix::E0,
        },
    ), // PageDown
    (
        HidUsage::keyboard(0x4F),
        WinScancode {
            code: 0x4D,
            prefix: ScanPrefix::E0,
        },
    ), // ArrowRight
    (
        HidUsage::keyboard(0x50),
        WinScancode {
            code: 0x4B,
            prefix: ScanPrefix::E0,
        },
    ), // ArrowLeft
    (
        HidUsage::keyboard(0x51),
        WinScancode {
            code: 0x50,
            prefix: ScanPrefix::E0,
        },
    ), // ArrowDown
    (
        HidUsage::keyboard(0x52),
        WinScancode {
            code: 0x48,
            prefix: ScanPrefix::E0,
        },
    ), // ArrowUp
    (
        HidUsage::keyboard(0x53),
        WinScancode {
            code: 0x45,
            prefix: ScanPrefix::E0,
        },
    ), // NumLock
    (
        HidUsage::keyboard(0x54),
        WinScancode {
            code: 0x35,
            prefix: ScanPrefix::E0,
        },
    ), // NumpadDivide
    (
        HidUsage::keyboard(0x55),
        WinScancode {
            code: 0x37,
            prefix: ScanPrefix::None,
        },
    ), // NumpadMultiply
    (
        HidUsage::keyboard(0x56),
        WinScancode {
            code: 0x4A,
            prefix: ScanPrefix::None,
        },
    ), // NumpadSubtract
    (
        HidUsage::keyboard(0x57),
        WinScancode {
            code: 0x4E,
            prefix: ScanPrefix::None,
        },
    ), // NumpadAdd
    (
        HidUsage::keyboard(0x58),
        WinScancode {
            code: 0x1C,
            prefix: ScanPrefix::E0,
        },
    ), // NumpadEnter
    (
        HidUsage::keyboard(0x59),
        WinScancode {
            code: 0x4F,
            prefix: ScanPrefix::None,
        },
    ), // Numpad1
    (
        HidUsage::keyboard(0x5A),
        WinScancode {
            code: 0x50,
            prefix: ScanPrefix::None,
        },
    ), // Numpad2
    (
        HidUsage::keyboard(0x5B),
        WinScancode {
            code: 0x51,
            prefix: ScanPrefix::None,
        },
    ), // Numpad3
    (
        HidUsage::keyboard(0x5C),
        WinScancode {
            code: 0x4B,
            prefix: ScanPrefix::None,
        },
    ), // Numpad4
    (
        HidUsage::keyboard(0x5D),
        WinScancode {
            code: 0x4C,
            prefix: ScanPrefix::None,
        },
    ), // Numpad5
    (
        HidUsage::keyboard(0x5E),
        WinScancode {
            code: 0x4D,
            prefix: ScanPrefix::None,
        },
    ), // Numpad6
    (
        HidUsage::keyboard(0x5F),
        WinScancode {
            code: 0x47,
            prefix: ScanPrefix::None,
        },
    ), // Numpad7
    (
        HidUsage::keyboard(0x60),
        WinScancode {
            code: 0x48,
            prefix: ScanPrefix::None,
        },
    ), // Numpad8
    (
        HidUsage::keyboard(0x61),
        WinScancode {
            code: 0x49,
            prefix: ScanPrefix::None,
        },
    ), // Numpad9
    (
        HidUsage::keyboard(0x62),
        WinScancode {
            code: 0x52,
            prefix: ScanPrefix::None,
        },
    ), // Numpad0
    (
        HidUsage::keyboard(0x63),
        WinScancode {
            code: 0x53,
            prefix: ScanPrefix::None,
        },
    ), // NumpadDecimal
    (
        HidUsage::keyboard(0x64),
        WinScancode {
            code: 0x56,
            prefix: ScanPrefix::None,
        },
    ), // IntlBackslash
    (
        HidUsage::keyboard(0x65),
        WinScancode {
            code: 0x5D,
            prefix: ScanPrefix::E0,
        },
    ), // ContextMenu
    (
        HidUsage::keyboard(0x66),
        WinScancode {
            code: 0x5E,
            prefix: ScanPrefix::E0,
        },
    ), // Power
    (
        HidUsage::keyboard(0x67),
        WinScancode {
            code: 0x59,
            prefix: ScanPrefix::None,
        },
    ), // NumpadEqual
    (
        HidUsage::keyboard(0x68),
        WinScancode {
            code: 0x64,
            prefix: ScanPrefix::None,
        },
    ), // F13
    (
        HidUsage::keyboard(0x69),
        WinScancode {
            code: 0x65,
            prefix: ScanPrefix::None,
        },
    ), // F14
    (
        HidUsage::keyboard(0x6A),
        WinScancode {
            code: 0x66,
            prefix: ScanPrefix::None,
        },
    ), // F15
    (
        HidUsage::keyboard(0x6B),
        WinScancode {
            code: 0x67,
            prefix: ScanPrefix::None,
        },
    ), // F16
    (
        HidUsage::keyboard(0x6C),
        WinScancode {
            code: 0x68,
            prefix: ScanPrefix::None,
        },
    ), // F17
    (
        HidUsage::keyboard(0x6D),
        WinScancode {
            code: 0x69,
            prefix: ScanPrefix::None,
        },
    ), // F18
    (
        HidUsage::keyboard(0x6E),
        WinScancode {
            code: 0x6A,
            prefix: ScanPrefix::None,
        },
    ), // F19
    (
        HidUsage::keyboard(0x6F),
        WinScancode {
            code: 0x6B,
            prefix: ScanPrefix::None,
        },
    ), // F20
    (
        HidUsage::keyboard(0x70),
        WinScancode {
            code: 0x6C,
            prefix: ScanPrefix::None,
        },
    ), // F21
    (
        HidUsage::keyboard(0x71),
        WinScancode {
            code: 0x6D,
            prefix: ScanPrefix::None,
        },
    ), // F22
    (
        HidUsage::keyboard(0x72),
        WinScancode {
            code: 0x6E,
            prefix: ScanPrefix::None,
        },
    ), // F23
    (
        HidUsage::keyboard(0x73),
        WinScancode {
            code: 0x76,
            prefix: ScanPrefix::None,
        },
    ), // F24
    (
        HidUsage::keyboard(0x75),
        WinScancode {
            code: 0x3B,
            prefix: ScanPrefix::E0,
        },
    ), // Help
    (
        HidUsage::keyboard(0x7A),
        WinScancode {
            code: 0x08,
            prefix: ScanPrefix::E0,
        },
    ), // Undo
    (
        HidUsage::keyboard(0x7B),
        WinScancode {
            code: 0x17,
            prefix: ScanPrefix::E0,
        },
    ), // Cut
    (
        HidUsage::keyboard(0x7C),
        WinScancode {
            code: 0x18,
            prefix: ScanPrefix::E0,
        },
    ), // Copy
    (
        HidUsage::keyboard(0x7D),
        WinScancode {
            code: 0x0A,
            prefix: ScanPrefix::E0,
        },
    ), // Paste
    (
        HidUsage::keyboard(0x7F),
        WinScancode {
            code: 0x20,
            prefix: ScanPrefix::E0,
        },
    ), // AudioVolumeMute
    (
        HidUsage::keyboard(0x80),
        WinScancode {
            code: 0x30,
            prefix: ScanPrefix::E0,
        },
    ), // AudioVolumeUp
    (
        HidUsage::keyboard(0x81),
        WinScancode {
            code: 0x2E,
            prefix: ScanPrefix::E0,
        },
    ), // AudioVolumeDown
    (
        HidUsage::keyboard(0x85),
        WinScancode {
            code: 0x7E,
            prefix: ScanPrefix::None,
        },
    ), // NumpadComma
    (
        HidUsage::keyboard(0x87),
        WinScancode {
            code: 0x73,
            prefix: ScanPrefix::None,
        },
    ), // IntlRo
    (
        HidUsage::keyboard(0x88),
        WinScancode {
            code: 0x70,
            prefix: ScanPrefix::None,
        },
    ), // KanaMode
    (
        HidUsage::keyboard(0x89),
        WinScancode {
            code: 0x7D,
            prefix: ScanPrefix::None,
        },
    ), // IntlYen
    (
        HidUsage::keyboard(0x8A),
        WinScancode {
            code: 0x79,
            prefix: ScanPrefix::None,
        },
    ), // Convert
    (
        HidUsage::keyboard(0x8B),
        WinScancode {
            code: 0x7B,
            prefix: ScanPrefix::None,
        },
    ), // NonConvert
    (
        HidUsage::keyboard(0x90),
        WinScancode {
            code: 0x72,
            prefix: ScanPrefix::None,
        },
    ), // Lang1
    (
        HidUsage::keyboard(0x91),
        WinScancode {
            code: 0x71,
            prefix: ScanPrefix::None,
        },
    ), // Lang2
    (
        HidUsage::keyboard(0x92),
        WinScancode {
            code: 0x78,
            prefix: ScanPrefix::None,
        },
    ), // Lang3
    (
        HidUsage::keyboard(0x93),
        WinScancode {
            code: 0x77,
            prefix: ScanPrefix::None,
        },
    ), // Lang4
    (
        HidUsage::keyboard(0xE0),
        WinScancode {
            code: 0x1D,
            prefix: ScanPrefix::None,
        },
    ), // ControlLeft
    (
        HidUsage::keyboard(0xE1),
        WinScancode {
            code: 0x2A,
            prefix: ScanPrefix::None,
        },
    ), // ShiftLeft
    (
        HidUsage::keyboard(0xE2),
        WinScancode {
            code: 0x38,
            prefix: ScanPrefix::None,
        },
    ), // AltLeft
    (
        HidUsage::keyboard(0xE3),
        WinScancode {
            code: 0x5B,
            prefix: ScanPrefix::E0,
        },
    ), // MetaLeft
    (
        HidUsage::keyboard(0xE4),
        WinScancode {
            code: 0x1D,
            prefix: ScanPrefix::E0,
        },
    ), // ControlRight
    (
        HidUsage::keyboard(0xE5),
        WinScancode {
            code: 0x36,
            prefix: ScanPrefix::None,
        },
    ), // ShiftRight
    (
        HidUsage::keyboard(0xE6),
        WinScancode {
            code: 0x38,
            prefix: ScanPrefix::E0,
        },
    ), // AltRight
    (
        HidUsage::keyboard(0xE7),
        WinScancode {
            code: 0x5C,
            prefix: ScanPrefix::E0,
        },
    ), // MetaRight
];

/// The Windows scan code for a HID usage, if Windows has one.
pub fn hid_to_windows(usage: HidUsage) -> Option<WinScancode> {
    TABLE.iter().find(|(u, _)| *u == usage).map(|&(_, sc)| sc)
}

/// The HID usage for a Windows scan code, if it maps to one.
pub fn windows_to_hid(scancode: WinScancode) -> Option<HidUsage> {
    TABLE
        .iter()
        .find(|(_, sc)| *sc == scancode)
        .map(|&(u, _)| u)
}
