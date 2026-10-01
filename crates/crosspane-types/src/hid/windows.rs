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
const TABLE: &[(HidUsage, WinScancode)] = &[];

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
