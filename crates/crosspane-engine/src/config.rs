//! Engine configuration.

use core::time::Duration;
use std::collections::BTreeMap;

use crosspane_input::accel::AccelProfile;
use crosspane_input::layout::LayoutOptions;
use crosspane_input::remap::RemapProfile;
use crosspane_platform::Chord;
use crosspane_types::hid::HidUsage;
use crosspane_types::id::NodeId;

#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// This node.
    pub node: NodeId,
    /// The release chord (04 §6), default Ctrl+Alt+Shift+Esc.
    pub release_chord: Chord,
    /// Holding the release chord this long is panic (04 §6). Default 1 s.
    pub panic_hold: Duration,
    /// The pointer must keep pressing a portal this long before crossing (03 §3; 0–200 ms).
    pub push_to_cross: Duration,
    /// DRAG-v0: offer the drag-across gesture (also needs `DRAG_FEATURE` on both sides).
    pub drag_across: bool,
    /// How long a window drag must push against a portal before crossing (DRAG-v0 D-3).
    pub drag_push_to_cross: Duration,
    /// Distance into the peer, perpendicular to the crossed edge, in the destination display's
    /// logical pixels, before a drag commits (DRAG-v0 §3).
    pub drag_commit_distance: f64,
    pub layout: LayoutOptions,
    pub accel: AccelProfile,
    /// Injection pauses this long after the last local physical input on a target (04 §6).
    pub local_override_pause: Duration,
    /// Modifier remap profile per target (WP-1.31); targets not listed get `RemapProfile::None`.
    pub remap: BTreeMap<NodeId, RemapProfile>,
}

impl EngineConfig {
    /// Defaults for `node`.
    pub fn new(node: NodeId) -> EngineConfig {
        EngineConfig {
            node,
            release_chord: Chord {
                modifiers: vec![
                    HidUsage::keyboard(0xE0), // Left Control
                    HidUsage::keyboard(0xE1), // Left Shift
                    HidUsage::keyboard(0xE2), // Left Alt
                ],
                key: HidUsage::keyboard(0x29), // Escape
            },
            panic_hold: Duration::from_secs(1),
            push_to_cross: Duration::ZERO,
            drag_across: true,
            drag_push_to_cross: Duration::from_millis(250),
            drag_commit_distance: 48.0,
            layout: LayoutOptions::default(),
            accel: AccelProfile::default(),
            local_override_pause: Duration::from_secs(1),
            remap: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_defaults_match_freeze() {
        let config = EngineConfig::new(NodeId([0; 32]));
        assert!(config.drag_across);
        assert_eq!(config.drag_push_to_cross, Duration::from_millis(250));
        assert_eq!(config.drag_commit_distance, 48.0);
        assert_eq!(config.push_to_cross, Duration::ZERO);
    }
}
