//! Canonical E1 input logic, OS-free (03 §3, 04 §8): the layout canvas and edge portals, pointer
//! tracking and acceleration, the key→node router, and the lease/heartbeat/journal reliability
//! contract. Everything here is a deterministic state machine driven by explicit timestamps, so it
//! runs unchanged in `crosspane-testkit` simulations.

#![deny(unsafe_code)]

pub mod accel;
pub mod journal;
pub mod layout;
pub mod lease;
pub mod router;

pub use crosspane_platform::{Edge, MotionKind, PortalId};

use crosspane_types::hid::{HidUsage, MouseButton};

/// Something that can be held down: a key or a pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Held {
    Key(HidUsage),
    Button(MouseButton),
}

/// Reliability timings (04 §8). Frozen.
pub mod timing {
    use core::time::Duration;

    /// Heartbeat interval while anything is held on the target.
    pub const HEARTBEAT_HELD: Duration = Duration::from_millis(50);
    /// Heartbeat interval otherwise.
    pub const HEARTBEAT_IDLE: Duration = Duration::from_millis(250);
    /// The target releases everything if no heartbeat arrives within this while keys are held.
    pub const LEASE_TIMEOUT: Duration = Duration::from_millis(300);
    /// The controller takes input back if an acknowledgement is older than
    /// `max(ACK_TIMEOUT_MIN, ACK_TIMEOUT_RTT_FACTOR × RTT)`.
    pub const ACK_TIMEOUT_MIN: Duration = Duration::from_millis(150);
    pub const ACK_TIMEOUT_RTT_FACTOR: u32 = 4;
}
