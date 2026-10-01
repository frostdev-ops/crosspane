//! HID usage ↔ Linux evdev `KEY_*` code (`linux/input-event-codes.h`). XKB keycode = evdev + 8.
//! The table is filled in by WP-0.4.

use super::HidUsage;

/// (HID usage, evdev code) pairs.
const TABLE: &[(HidUsage, u16)] = &[];

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
