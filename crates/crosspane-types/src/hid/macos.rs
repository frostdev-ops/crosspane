//! HID usage ↔ macOS virtual key code (`kVK_*` in HIToolbox `Events.h`).
//! Keyboard-page mappings from Chromium's `dom_code_data.inc`, cross-checked against `Events.h`.

use super::HidUsage;

/// (HID usage, kVK code) pairs.
const TABLE: &[(HidUsage, u16)] = &[
    (HidUsage::keyboard(0x04), 0x00), // KeyA, kVK_ANSI_A
    (HidUsage::keyboard(0x05), 0x0B), // KeyB, kVK_ANSI_B
    (HidUsage::keyboard(0x06), 0x08), // KeyC, kVK_ANSI_C
    (HidUsage::keyboard(0x07), 0x02), // KeyD, kVK_ANSI_D
    (HidUsage::keyboard(0x08), 0x0E), // KeyE, kVK_ANSI_E
    (HidUsage::keyboard(0x09), 0x03), // KeyF, kVK_ANSI_F
    (HidUsage::keyboard(0x0A), 0x05), // KeyG, kVK_ANSI_G
    (HidUsage::keyboard(0x0B), 0x04), // KeyH, kVK_ANSI_H
    (HidUsage::keyboard(0x0C), 0x22), // KeyI, kVK_ANSI_I
    (HidUsage::keyboard(0x0D), 0x26), // KeyJ, kVK_ANSI_J
    (HidUsage::keyboard(0x0E), 0x28), // KeyK, kVK_ANSI_K
    (HidUsage::keyboard(0x0F), 0x25), // KeyL, kVK_ANSI_L
    (HidUsage::keyboard(0x10), 0x2E), // KeyM, kVK_ANSI_M
    (HidUsage::keyboard(0x11), 0x2D), // KeyN, kVK_ANSI_N
    (HidUsage::keyboard(0x12), 0x1F), // KeyO, kVK_ANSI_O
    (HidUsage::keyboard(0x13), 0x23), // KeyP, kVK_ANSI_P
    (HidUsage::keyboard(0x14), 0x0C), // KeyQ, kVK_ANSI_Q
    (HidUsage::keyboard(0x15), 0x0F), // KeyR, kVK_ANSI_R
    (HidUsage::keyboard(0x16), 0x01), // KeyS, kVK_ANSI_S
    (HidUsage::keyboard(0x17), 0x11), // KeyT, kVK_ANSI_T
    (HidUsage::keyboard(0x18), 0x20), // KeyU, kVK_ANSI_U
    (HidUsage::keyboard(0x19), 0x09), // KeyV, kVK_ANSI_V
    (HidUsage::keyboard(0x1A), 0x0D), // KeyW, kVK_ANSI_W
    (HidUsage::keyboard(0x1B), 0x07), // KeyX, kVK_ANSI_X
    (HidUsage::keyboard(0x1C), 0x10), // KeyY, kVK_ANSI_Y
    (HidUsage::keyboard(0x1D), 0x06), // KeyZ, kVK_ANSI_Z
    (HidUsage::keyboard(0x1E), 0x12), // Digit1, kVK_ANSI_1
    (HidUsage::keyboard(0x1F), 0x13), // Digit2, kVK_ANSI_2
    (HidUsage::keyboard(0x20), 0x14), // Digit3, kVK_ANSI_3
    (HidUsage::keyboard(0x21), 0x15), // Digit4, kVK_ANSI_4
    (HidUsage::keyboard(0x22), 0x17), // Digit5, kVK_ANSI_5
    (HidUsage::keyboard(0x23), 0x16), // Digit6, kVK_ANSI_6
    (HidUsage::keyboard(0x24), 0x1A), // Digit7, kVK_ANSI_7
    (HidUsage::keyboard(0x25), 0x1C), // Digit8, kVK_ANSI_8
    (HidUsage::keyboard(0x26), 0x19), // Digit9, kVK_ANSI_9
    (HidUsage::keyboard(0x27), 0x1D), // Digit0, kVK_ANSI_0
    (HidUsage::keyboard(0x28), 0x24), // Enter, kVK_Return
    (HidUsage::keyboard(0x29), 0x35), // Escape, kVK_Escape
    (HidUsage::keyboard(0x2A), 0x33), // Backspace, kVK_Delete
    (HidUsage::keyboard(0x2B), 0x30), // Tab, kVK_Tab
    (HidUsage::keyboard(0x2C), 0x31), // Space, kVK_Space
    (HidUsage::keyboard(0x2D), 0x1B), // Minus, kVK_ANSI_Minus
    (HidUsage::keyboard(0x2E), 0x18), // Equal, kVK_ANSI_Equal
    (HidUsage::keyboard(0x2F), 0x21), // BracketLeft, kVK_ANSI_LeftBracket
    (HidUsage::keyboard(0x30), 0x1E), // BracketRight, kVK_ANSI_RightBracket
    (HidUsage::keyboard(0x31), 0x2A), // Backslash, kVK_ANSI_Backslash
    (HidUsage::keyboard(0x33), 0x29), // Semicolon, kVK_ANSI_Semicolon
    (HidUsage::keyboard(0x34), 0x27), // Quote, kVK_ANSI_Quote
    (HidUsage::keyboard(0x35), 0x32), // Backquote, kVK_ANSI_Grave
    (HidUsage::keyboard(0x36), 0x2B), // Comma, kVK_ANSI_Comma
    (HidUsage::keyboard(0x37), 0x2F), // Period, kVK_ANSI_Period
    (HidUsage::keyboard(0x38), 0x2C), // Slash, kVK_ANSI_Slash
    (HidUsage::keyboard(0x39), 0x39), // CapsLock, kVK_CapsLock
    (HidUsage::keyboard(0x3A), 0x7A), // F1, kVK_F1
    (HidUsage::keyboard(0x3B), 0x78), // F2, kVK_F2
    (HidUsage::keyboard(0x3C), 0x63), // F3, kVK_F3
    (HidUsage::keyboard(0x3D), 0x76), // F4, kVK_F4
    (HidUsage::keyboard(0x3E), 0x60), // F5, kVK_F5
    (HidUsage::keyboard(0x3F), 0x61), // F6, kVK_F6
    (HidUsage::keyboard(0x40), 0x62), // F7, kVK_F7
    (HidUsage::keyboard(0x41), 0x64), // F8, kVK_F8
    (HidUsage::keyboard(0x42), 0x65), // F9, kVK_F9
    (HidUsage::keyboard(0x43), 0x6D), // F10, kVK_F10
    (HidUsage::keyboard(0x44), 0x67), // F11, kVK_F11
    (HidUsage::keyboard(0x45), 0x6F), // F12, kVK_F12
    (HidUsage::keyboard(0x49), 0x72), // Insert, kVK_Help
    (HidUsage::keyboard(0x4A), 0x73), // Home, kVK_Home
    (HidUsage::keyboard(0x4B), 0x74), // PageUp, kVK_PageUp
    (HidUsage::keyboard(0x4C), 0x75), // Delete, kVK_ForwardDelete
    (HidUsage::keyboard(0x4D), 0x77), // End, kVK_End
    (HidUsage::keyboard(0x4E), 0x79), // PageDown, kVK_PageDown
    (HidUsage::keyboard(0x4F), 0x7C), // ArrowRight, kVK_RightArrow
    (HidUsage::keyboard(0x50), 0x7B), // ArrowLeft, kVK_LeftArrow
    (HidUsage::keyboard(0x51), 0x7D), // ArrowDown, kVK_DownArrow
    (HidUsage::keyboard(0x52), 0x7E), // ArrowUp, kVK_UpArrow
    (HidUsage::keyboard(0x53), 0x47), // NumLock, kVK_ANSI_KeypadClear
    (HidUsage::keyboard(0x54), 0x4B), // NumpadDivide, kVK_ANSI_KeypadDivide
    (HidUsage::keyboard(0x55), 0x43), // NumpadMultiply, kVK_ANSI_KeypadMultiply
    (HidUsage::keyboard(0x56), 0x4E), // NumpadSubtract, kVK_ANSI_KeypadMinus
    (HidUsage::keyboard(0x57), 0x45), // NumpadAdd, kVK_ANSI_KeypadPlus
    (HidUsage::keyboard(0x58), 0x4C), // NumpadEnter, kVK_ANSI_KeypadEnter
    (HidUsage::keyboard(0x59), 0x53), // Numpad1, kVK_ANSI_Keypad1
    (HidUsage::keyboard(0x5A), 0x54), // Numpad2, kVK_ANSI_Keypad2
    (HidUsage::keyboard(0x5B), 0x55), // Numpad3, kVK_ANSI_Keypad3
    (HidUsage::keyboard(0x5C), 0x56), // Numpad4, kVK_ANSI_Keypad4
    (HidUsage::keyboard(0x5D), 0x57), // Numpad5, kVK_ANSI_Keypad5
    (HidUsage::keyboard(0x5E), 0x58), // Numpad6, kVK_ANSI_Keypad6
    (HidUsage::keyboard(0x5F), 0x59), // Numpad7, kVK_ANSI_Keypad7
    (HidUsage::keyboard(0x60), 0x5B), // Numpad8, kVK_ANSI_Keypad8
    (HidUsage::keyboard(0x61), 0x5C), // Numpad9, kVK_ANSI_Keypad9
    (HidUsage::keyboard(0x62), 0x52), // Numpad0, kVK_ANSI_Keypad0
    (HidUsage::keyboard(0x63), 0x41), // NumpadDecimal, kVK_ANSI_KeypadDecimal
    (HidUsage::keyboard(0x64), 0x0A), // IntlBackslash, kVK_ISO_Section
    (HidUsage::keyboard(0x65), 0x6E), // ContextMenu, kVK_ContextualMenu
    (HidUsage::keyboard(0x67), 0x51), // NumpadEqual, kVK_ANSI_KeypadEquals
    (HidUsage::keyboard(0x68), 0x69), // F13, kVK_F13
    (HidUsage::keyboard(0x69), 0x6B), // F14, kVK_F14
    (HidUsage::keyboard(0x6A), 0x71), // F15, kVK_F15
    (HidUsage::keyboard(0x6B), 0x6A), // F16, kVK_F16
    (HidUsage::keyboard(0x6C), 0x40), // F17, kVK_F17
    (HidUsage::keyboard(0x6D), 0x4F), // F18, kVK_F18
    (HidUsage::keyboard(0x6E), 0x50), // F19, kVK_F19
    (HidUsage::keyboard(0x6F), 0x5A), // F20, kVK_F20
    (HidUsage::keyboard(0x7F), 0x4A), // AudioVolumeMute, kVK_Mute
    (HidUsage::keyboard(0x80), 0x48), // AudioVolumeUp, kVK_VolumeUp
    (HidUsage::keyboard(0x81), 0x49), // AudioVolumeDown, kVK_VolumeDown
    (HidUsage::keyboard(0x85), 0x5F), // NumpadComma, kVK_JIS_KeypadComma
    (HidUsage::keyboard(0x87), 0x5E), // IntlRo, kVK_JIS_Underscore
    (HidUsage::keyboard(0x89), 0x5D), // IntlYen, kVK_JIS_Yen
    (HidUsage::keyboard(0x90), 0x68), // Lang1, kVK_JIS_Kana
    (HidUsage::keyboard(0x91), 0x66), // Lang2, kVK_JIS_Eisu
    (HidUsage::keyboard(0xE0), 0x3B), // ControlLeft, kVK_Control
    (HidUsage::keyboard(0xE1), 0x38), // ShiftLeft, kVK_Shift
    (HidUsage::keyboard(0xE2), 0x3A), // AltLeft, kVK_Option
    (HidUsage::keyboard(0xE3), 0x37), // MetaLeft, kVK_Command
    (HidUsage::keyboard(0xE4), 0x3E), // ControlRight, kVK_RightControl
    (HidUsage::keyboard(0xE5), 0x3C), // ShiftRight, kVK_RightShift
    (HidUsage::keyboard(0xE6), 0x3D), // AltRight, kVK_RightOption
    (HidUsage::keyboard(0xE7), 0x36), // MetaRight, kVK_RightCommand
];

/// The macOS virtual key code for a HID usage, if macOS has one.
pub fn hid_to_macos(usage: HidUsage) -> Option<u16> {
    TABLE
        .iter()
        .find(|(u, _)| *u == usage)
        .map(|&(_, code)| code)
}

/// The HID usage for a macOS virtual key code, if it maps to one.
pub fn macos_to_hid(kvk: u16) -> Option<HidUsage> {
    TABLE.iter().find(|(_, c)| *c == kvk).map(|&(u, _)| u)
}
