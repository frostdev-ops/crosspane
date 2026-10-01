//! HID usage ↔ macOS virtual key code (`kVK_*` in HIToolbox `Events.h`). The table is filled in by
//! WP-0.3.

use super::HidUsage;

/// (HID usage, kVK code) pairs.
const TABLE: &[(HidUsage, u16)] = &[];

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
