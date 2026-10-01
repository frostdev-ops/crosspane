//! HID usage ↔ Linux evdev `KEY_*` code (`linux/input-event-codes.h`). XKB keycode = evdev + 8.
//! Keyboard/Keypad mappings from Chromium's `ui/events/keycodes/dom/dom_code_data.inc`
//! (BSD-3-Clause), cross-checked against `linux/input-event-codes.h`. Rows without a DOM
//! code string use Chromium's enum name in their comment.

use super::HidUsage;

/// (HID usage, evdev code) pairs.
const TABLE: &[(HidUsage, u16)] = &[
    (HidUsage::keyboard(0x04), 30),  // KeyA, KEY_A
    (HidUsage::keyboard(0x05), 48),  // KeyB, KEY_B
    (HidUsage::keyboard(0x06), 46),  // KeyC, KEY_C
    (HidUsage::keyboard(0x07), 32),  // KeyD, KEY_D
    (HidUsage::keyboard(0x08), 18),  // KeyE, KEY_E
    (HidUsage::keyboard(0x09), 33),  // KeyF, KEY_F
    (HidUsage::keyboard(0x0A), 34),  // KeyG, KEY_G
    (HidUsage::keyboard(0x0B), 35),  // KeyH, KEY_H
    (HidUsage::keyboard(0x0C), 23),  // KeyI, KEY_I
    (HidUsage::keyboard(0x0D), 36),  // KeyJ, KEY_J
    (HidUsage::keyboard(0x0E), 37),  // KeyK, KEY_K
    (HidUsage::keyboard(0x0F), 38),  // KeyL, KEY_L
    (HidUsage::keyboard(0x10), 50),  // KeyM, KEY_M
    (HidUsage::keyboard(0x11), 49),  // KeyN, KEY_N
    (HidUsage::keyboard(0x12), 24),  // KeyO, KEY_O
    (HidUsage::keyboard(0x13), 25),  // KeyP, KEY_P
    (HidUsage::keyboard(0x14), 16),  // KeyQ, KEY_Q
    (HidUsage::keyboard(0x15), 19),  // KeyR, KEY_R
    (HidUsage::keyboard(0x16), 31),  // KeyS, KEY_S
    (HidUsage::keyboard(0x17), 20),  // KeyT, KEY_T
    (HidUsage::keyboard(0x18), 22),  // KeyU, KEY_U
    (HidUsage::keyboard(0x19), 47),  // KeyV, KEY_V
    (HidUsage::keyboard(0x1A), 17),  // KeyW, KEY_W
    (HidUsage::keyboard(0x1B), 45),  // KeyX, KEY_X
    (HidUsage::keyboard(0x1C), 21),  // KeyY, KEY_Y
    (HidUsage::keyboard(0x1D), 44),  // KeyZ, KEY_Z
    (HidUsage::keyboard(0x1E), 2),   // Digit1, KEY_1
    (HidUsage::keyboard(0x1F), 3),   // Digit2, KEY_2
    (HidUsage::keyboard(0x20), 4),   // Digit3, KEY_3
    (HidUsage::keyboard(0x21), 5),   // Digit4, KEY_4
    (HidUsage::keyboard(0x22), 6),   // Digit5, KEY_5
    (HidUsage::keyboard(0x23), 7),   // Digit6, KEY_6
    (HidUsage::keyboard(0x24), 8),   // Digit7, KEY_7
    (HidUsage::keyboard(0x25), 9),   // Digit8, KEY_8
    (HidUsage::keyboard(0x26), 10),  // Digit9, KEY_9
    (HidUsage::keyboard(0x27), 11),  // Digit0, KEY_0
    (HidUsage::keyboard(0x28), 28),  // Enter, KEY_ENTER
    (HidUsage::keyboard(0x29), 1),   // Escape, KEY_ESC
    (HidUsage::keyboard(0x2A), 14),  // Backspace, KEY_BACKSPACE
    (HidUsage::keyboard(0x2B), 15),  // Tab, KEY_TAB
    (HidUsage::keyboard(0x2C), 57),  // Space, KEY_SPACE
    (HidUsage::keyboard(0x2D), 12),  // Minus, KEY_MINUS
    (HidUsage::keyboard(0x2E), 13),  // Equal, KEY_EQUAL
    (HidUsage::keyboard(0x2F), 26),  // BracketLeft, KEY_LEFTBRACE
    (HidUsage::keyboard(0x30), 27),  // BracketRight, KEY_RIGHTBRACE
    (HidUsage::keyboard(0x31), 43),  // Backslash, KEY_BACKSLASH
    (HidUsage::keyboard(0x33), 39),  // Semicolon, KEY_SEMICOLON
    (HidUsage::keyboard(0x34), 40),  // Quote, KEY_APOSTROPHE
    (HidUsage::keyboard(0x35), 41),  // Backquote, KEY_GRAVE
    (HidUsage::keyboard(0x36), 51),  // Comma, KEY_COMMA
    (HidUsage::keyboard(0x37), 52),  // Period, KEY_DOT
    (HidUsage::keyboard(0x38), 53),  // Slash, KEY_SLASH
    (HidUsage::keyboard(0x39), 58),  // CapsLock, KEY_CAPSLOCK
    (HidUsage::keyboard(0x3A), 59),  // F1, KEY_F1
    (HidUsage::keyboard(0x3B), 60),  // F2, KEY_F2
    (HidUsage::keyboard(0x3C), 61),  // F3, KEY_F3
    (HidUsage::keyboard(0x3D), 62),  // F4, KEY_F4
    (HidUsage::keyboard(0x3E), 63),  // F5, KEY_F5
    (HidUsage::keyboard(0x3F), 64),  // F6, KEY_F6
    (HidUsage::keyboard(0x40), 65),  // F7, KEY_F7
    (HidUsage::keyboard(0x41), 66),  // F8, KEY_F8
    (HidUsage::keyboard(0x42), 67),  // F9, KEY_F9
    (HidUsage::keyboard(0x43), 68),  // F10, KEY_F10
    (HidUsage::keyboard(0x44), 87),  // F11, KEY_F11
    (HidUsage::keyboard(0x45), 88),  // F12, KEY_F12
    (HidUsage::keyboard(0x46), 99),  // PrintScreen, KEY_SYSRQ
    (HidUsage::keyboard(0x47), 70),  // ScrollLock, KEY_SCROLLLOCK
    (HidUsage::keyboard(0x48), 119), // Pause, KEY_PAUSE
    (HidUsage::keyboard(0x49), 110), // Insert, KEY_INSERT
    (HidUsage::keyboard(0x4A), 102), // Home, KEY_HOME
    (HidUsage::keyboard(0x4B), 104), // PageUp, KEY_PAGEUP
    (HidUsage::keyboard(0x4C), 111), // Delete, KEY_DELETE
    (HidUsage::keyboard(0x4D), 107), // End, KEY_END
    (HidUsage::keyboard(0x4E), 109), // PageDown, KEY_PAGEDOWN
    (HidUsage::keyboard(0x4F), 106), // ArrowRight, KEY_RIGHT
    (HidUsage::keyboard(0x50), 105), // ArrowLeft, KEY_LEFT
    (HidUsage::keyboard(0x51), 108), // ArrowDown, KEY_DOWN
    (HidUsage::keyboard(0x52), 103), // ArrowUp, KEY_UP
    (HidUsage::keyboard(0x53), 69),  // NumLock, KEY_NUMLOCK
    (HidUsage::keyboard(0x54), 98),  // NumpadDivide, KEY_KPSLASH
    (HidUsage::keyboard(0x55), 55),  // NumpadMultiply, KEY_KPASTERISK
    (HidUsage::keyboard(0x56), 74),  // NumpadSubtract, KEY_KPMINUS
    (HidUsage::keyboard(0x57), 78),  // NumpadAdd, KEY_KPPLUS
    (HidUsage::keyboard(0x58), 96),  // NumpadEnter, KEY_KPENTER
    (HidUsage::keyboard(0x59), 79),  // Numpad1, KEY_KP1
    (HidUsage::keyboard(0x5A), 80),  // Numpad2, KEY_KP2
    (HidUsage::keyboard(0x5B), 81),  // Numpad3, KEY_KP3
    (HidUsage::keyboard(0x5C), 75),  // Numpad4, KEY_KP4
    (HidUsage::keyboard(0x5D), 76),  // Numpad5, KEY_KP5
    (HidUsage::keyboard(0x5E), 77),  // Numpad6, KEY_KP6
    (HidUsage::keyboard(0x5F), 71),  // Numpad7, KEY_KP7
    (HidUsage::keyboard(0x60), 72),  // Numpad8, KEY_KP8
    (HidUsage::keyboard(0x61), 73),  // Numpad9, KEY_KP9
    (HidUsage::keyboard(0x62), 82),  // Numpad0, KEY_KP0
    (HidUsage::keyboard(0x63), 83),  // NumpadDecimal, KEY_KPDOT
    (HidUsage::keyboard(0x64), 86),  // IntlBackslash, KEY_102ND
    (HidUsage::keyboard(0x65), 127), // ContextMenu, KEY_COMPOSE
    (HidUsage::keyboard(0x66), 116), // Power, KEY_POWER
    (HidUsage::keyboard(0x67), 117), // NumpadEqual, KEY_KPEQUAL
    (HidUsage::keyboard(0x68), 183), // F13, KEY_F13
    (HidUsage::keyboard(0x69), 184), // F14, KEY_F14
    (HidUsage::keyboard(0x6A), 185), // F15, KEY_F15
    (HidUsage::keyboard(0x6B), 186), // F16, KEY_F16
    (HidUsage::keyboard(0x6C), 187), // F17, KEY_F17
    (HidUsage::keyboard(0x6D), 188), // F18, KEY_F18
    (HidUsage::keyboard(0x6E), 189), // F19, KEY_F19
    (HidUsage::keyboard(0x6F), 190), // F20, KEY_F20
    (HidUsage::keyboard(0x70), 191), // F21, KEY_F21
    (HidUsage::keyboard(0x71), 192), // F22, KEY_F22
    (HidUsage::keyboard(0x72), 193), // F23, KEY_F23
    (HidUsage::keyboard(0x73), 194), // F24, KEY_F24
    (HidUsage::keyboard(0x74), 134), // Open, KEY_OPEN
    (HidUsage::keyboard(0x75), 138), // Help, KEY_HELP
    (HidUsage::keyboard(0x77), 132), // Select, KEY_FRONT
    (HidUsage::keyboard(0x79), 129), // Again, KEY_AGAIN
    (HidUsage::keyboard(0x7A), 131), // Undo, KEY_UNDO
    (HidUsage::keyboard(0x7B), 137), // Cut, KEY_CUT
    (HidUsage::keyboard(0x7C), 133), // Copy, KEY_COPY
    (HidUsage::keyboard(0x7D), 135), // Paste, KEY_PASTE
    (HidUsage::keyboard(0x7E), 136), // Find, KEY_FIND
    (HidUsage::keyboard(0x7F), 113), // AudioVolumeMute, KEY_MUTE
    (HidUsage::keyboard(0x80), 115), // AudioVolumeUp, KEY_VOLUMEUP
    (HidUsage::keyboard(0x81), 114), // AudioVolumeDown, KEY_VOLUMEDOWN
    (HidUsage::keyboard(0x85), 121), // NumpadComma, KEY_KPCOMMA
    (HidUsage::keyboard(0x87), 89),  // IntlRo, KEY_RO
    (HidUsage::keyboard(0x88), 93),  // KanaMode, KEY_KATAKANAHIRAGANA
    (HidUsage::keyboard(0x89), 124), // IntlYen, KEY_YEN
    (HidUsage::keyboard(0x8A), 92),  // Convert, KEY_HENKAN
    (HidUsage::keyboard(0x8B), 94),  // NonConvert, KEY_MUHENKAN
    (HidUsage::keyboard(0x90), 122), // Lang1, KEY_HANGEUL
    (HidUsage::keyboard(0x91), 123), // Lang2, KEY_HANJA
    (HidUsage::keyboard(0x92), 90),  // Lang3, KEY_KATAKANA
    (HidUsage::keyboard(0x93), 91),  // Lang4, KEY_HIRAGANA
    (HidUsage::keyboard(0x94), 85),  // Lang5, KEY_ZENKAKUHANKAKU
    (HidUsage::keyboard(0xB6), 179), // NumpadParenLeft, KEY_KPLEFTPAREN
    (HidUsage::keyboard(0xB7), 180), // NumpadParenRight, KEY_KPRIGHTPAREN
    (HidUsage::keyboard(0xD7), 118), // NUMPAD_SIGN_CHANGE, KEY_KPPLUSMINUS
    (HidUsage::keyboard(0xE0), 29),  // ControlLeft, KEY_LEFTCTRL
    (HidUsage::keyboard(0xE1), 42),  // ShiftLeft, KEY_LEFTSHIFT
    (HidUsage::keyboard(0xE2), 56),  // AltLeft, KEY_LEFTALT
    (HidUsage::keyboard(0xE3), 125), // MetaLeft, KEY_LEFTMETA
    (HidUsage::keyboard(0xE4), 97),  // ControlRight, KEY_RIGHTCTRL
    (HidUsage::keyboard(0xE5), 54),  // ShiftRight, KEY_RIGHTSHIFT
    (HidUsage::keyboard(0xE6), 100), // AltRight, KEY_RIGHTALT
    (HidUsage::keyboard(0xE7), 126), // MetaRight, KEY_RIGHTMETA
];

/// The evdev code for a HID usage, if Linux has one.
pub fn hid_to_evdev(usage: HidUsage) -> Option<u16> {
    TABLE
        .iter()
        .find(|(u, _)| *u == usage)
        .map(|&(_, code)| code)
}

/// The HID usage for an evdev code, if it maps to one.
pub fn evdev_to_hid(code: u16) -> Option<HidUsage> {
    TABLE.iter().find(|(_, c)| *c == code).map(|&(u, _)| u)
}
