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
        // WP-1.31 implements this.
        let _ = self;
        usage
    }
}
