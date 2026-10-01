//! Engine configuration.

use core::time::Duration;

use crosspane_input::accel::AccelProfile;
use crosspane_input::layout::LayoutOptions;
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
    pub layout: LayoutOptions,
    pub accel: AccelProfile,
    /// Injection pauses this long after the last local physical input on a target (04 §6).
    pub local_override_pause: Duration,
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
            layout: LayoutOptions::default(),
            accel: AccelProfile::default(),
            local_override_pause: Duration::from_secs(1),
        }
    }
}
