//! Canonical key and button codes (03 §3).
//!
//! Keys are USB HID usages. Each OS has a mapping table, one module per OS, built from the same
//! reference (Chromium's `dom_code_data.inc`) so the tables agree.

mod evdev;
mod macos;
mod windows;

use serde::{Deserialize, Serialize};

pub use evdev::{evdev_to_hid, hid_to_evdev};
pub use macos::{hid_to_macos, macos_to_hid};
pub use windows::{ScanPrefix, WinScancode, hid_to_windows, windows_to_hid};

/// A USB HID usage: usage page and usage ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HidUsage {
    pub page: u16,
    pub id: u16,
}

impl HidUsage {
    /// The Keyboard/Keypad usage page.
    pub const PAGE_KEYBOARD: u16 = 0x07;

    /// A usage on the Keyboard/Keypad page.
    pub const fn keyboard(id: u16) -> Self {
        HidUsage {
            page: Self::PAGE_KEYBOARD,
            id,
        }
    }

    /// True for the eight modifier keys (Left/Right Control, Shift, Alt, GUI).
    pub const fn is_modifier(self) -> bool {
        self.page == Self::PAGE_KEYBOARD && self.id >= 0xE0 && self.id <= 0xE7
    }
}

/// A pointer button, numbered as on the HID Button page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MouseButton(pub u8);

impl MouseButton {
    pub const PRIMARY: MouseButton = MouseButton(1);
    pub const SECONDARY: MouseButton = MouseButton(2);
    pub const TERTIARY: MouseButton = MouseButton(3);
    pub const BACK: MouseButton = MouseButton(4);
    pub const FORWARD: MouseButton = MouseButton(5);
}
