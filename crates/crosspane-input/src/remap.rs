//! Per-target modifier remap profiles (03 §3 "Keyboard layout", WP-1.31).
//!
//! Keys are forwarded as physical keys; a profile only swaps modifier keys, so shortcuts land on
//! the key the target's user would expect. The controller applies the profile of the node it is
//! controlling to each key **at press time**, and a release always repeats the mapping its press
//! got, even if the profile changed in between (04 §8 invariant 1).

use serde::{Deserialize, Serialize};

use crosspane_types::hid::HidUsage;

/// How the controller rewrites modifier keys for one target. Every profile is an involution
/// (applying it twice gives the original key) and maps only the eight modifier keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemapProfile {
    /// Forward keys unchanged.
    #[default]
    None,
    /// Swap Control and GUI (⌘ / Super / Windows key), left with left and right with right: a PC
    /// keyboard's Ctrl+C becomes ⌘C on a Mac, and a Mac keyboard's ⌘C becomes Ctrl+C on a PC.
    SwapCtrlGui,
    /// Swap Alt (⌥) and GUI, left with left and right with right: the key next to the space bar
    /// keeps its role when a PC keyboard drives a Mac or the other way round.
    SwapAltGui,
}

impl RemapProfile {
    /// The key to send for physical key `usage`. Non-modifier keys are unchanged.
    pub fn map(self, usage: HidUsage) -> HidUsage {
        if usage.page != HidUsage::PAGE_KEYBOARD {
            return usage;
        }
        let id = match (self, usage.id) {
            (Self::SwapCtrlGui, 0xE0) | (Self::SwapAltGui, 0xE2) => 0xE3,
            (Self::SwapCtrlGui, 0xE3) => 0xE0,
            (Self::SwapCtrlGui, 0xE4) | (Self::SwapAltGui, 0xE6) => 0xE7,
            (Self::SwapCtrlGui, 0xE7) => 0xE4,
            (Self::SwapAltGui, 0xE3) => 0xE2,
            (Self::SwapAltGui, 0xE7) => 0xE6,
            _ => usage.id,
        };
        HidUsage::keyboard(id)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const PROFILES: [RemapProfile; 3] = [
        RemapProfile::None,
        RemapProfile::SwapCtrlGui,
        RemapProfile::SwapAltGui,
    ];

    #[test]
    fn all_modifiers_for_each_profile() {
        let expected = [
            [0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7],
            [0xE3, 0xE1, 0xE2, 0xE0, 0xE7, 0xE5, 0xE6, 0xE4],
            [0xE0, 0xE1, 0xE3, 0xE2, 0xE4, 0xE5, 0xE7, 0xE6],
        ];
        for (profile, mapped) in PROFILES.into_iter().zip(expected) {
            for (physical, mapped) in (0xE0..=0xE7).zip(mapped) {
                assert_eq!(
                    profile.map(HidUsage::keyboard(physical)),
                    HidUsage::keyboard(mapped),
                    "{profile:?}: {physical:#x}"
                );
            }
        }
    }

    proptest! {
        #[test]
        fn profiles_are_involutions_and_preserve_non_modifiers(
            id in any::<u8>(),
            page in any::<u16>().prop_filter("non-keyboard page", |page| *page != HidUsage::PAGE_KEYBOARD),
        ) {
            let usage = HidUsage::keyboard(u16::from(id));
            let other_page = HidUsage { page, id: u16::from(id) };
            for profile in PROFILES {
                prop_assert_eq!(profile.map(profile.map(usage)), usage);
                if !usage.is_modifier() {
                    prop_assert_eq!(profile.map(usage), usage);
                }
                prop_assert_eq!(profile.map(other_page), other_page);
            }
        }
    }
}
