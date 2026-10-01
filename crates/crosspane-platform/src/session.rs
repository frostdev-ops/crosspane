//! Session state and the I/O gate (04 §7).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::{EventSink, PlatformError};

/// Whether the local session is locked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LockState {
    Unlocked,
    Locked,
    /// The backend can't prove the session is unlocked. Treated exactly like `Locked`.
    Unknown,
}

/// The state of the local graphical session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionState {
    pub lock: LockState,
    /// Whether this graphical session is the active one (fast user switching, 04 §7); `None` if
    /// unknown.
    pub active: Option<bool>,
}

impl SessionState {
    /// Capture and injection are permitted only when the session is provably unlocked and active.
    pub const fn permits_io(self) -> bool {
        matches!(self.lock, LockState::Unlocked) && matches!(self.active, Some(true))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionEvent {
    /// The current state: delivered first after `subscribe`, then on every change.
    State(SessionState),
    /// The machine is about to sleep, when the OS announces it.
    WillSleep,
    /// The machine woke. I/O stays forbidden until the backend has re-validated the state and sent a
    /// fresh `State`.
    Woke,
}

/// Lock, activity and sleep signals for the local graphical session.
///
/// The backend drives the session side of the [`IoGate`] itself, without waiting for the engine:
/// it closes the gate the moment it observes a lock, deactivation, sleep, a loss of its own
/// observation (which also yields `State` with `lock: Unknown`), or a wake not yet re-validated, and
/// opens it only when [`SessionState::permits_io`] holds.
pub trait SessionEvents: Send {
    /// The current state.
    fn state(&self) -> SessionState;

    /// Start delivering events to `sink`. Called once; events stop when the backend is dropped.
    fn subscribe(&mut self, sink: Arc<dyn EventSink<SessionEvent>>) -> Result<(), PlatformError>;
}

/// The single switch that permits capture and injection on this node (04 §7).
///
/// It is open only while both sides agree: the session backend (lock and activity state) and the
/// engine (e.g. closed after a panic). It starts closed. Injectors check it immediately before each
/// OS submission that could press, move or scroll, and capture checks it before activating and
/// ends itself when it closes. Releases (key and button ups, `release_all`, recovery) are allowed
/// while it is closed: they only ever release, and leaving a key held could auto-repeat into a lock
/// screen.
///
/// Both sides live in one atomic, so `is_open` sees a single consistent moment.
#[derive(Debug, Default)]
pub struct IoGate {
    /// Bit 0: the session permits I/O. Bit 1: the engine permits I/O.
    flags: AtomicU8,
}

impl IoGate {
    const SESSION: u8 = 0b01;
    const ENGINE: u8 = 0b10;

    /// A new, closed gate.
    pub fn new() -> Arc<Self> {
        Arc::new(IoGate::default())
    }

    /// True when both the session backend and the engine permit I/O.
    pub fn is_open(&self) -> bool {
        self.flags.load(Ordering::Acquire) & (Self::SESSION | Self::ENGINE)
            == Self::SESSION | Self::ENGINE
    }

    /// Set by the session backend only.
    pub fn set_session_permits(&self, permits: bool) {
        self.set(Self::SESSION, permits);
    }

    /// Set by the engine only.
    pub fn set_engine_permits(&self, permits: bool) {
        self.set(Self::ENGINE, permits);
    }

    fn set(&self, bit: u8, on: bool) {
        if on {
            self.flags.fetch_or(bit, Ordering::AcqRel);
        } else {
            self.flags.fetch_and(!bit, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_needs_both_sides() {
        let gate = IoGate::new();
        assert!(!gate.is_open());
        gate.set_session_permits(true);
        assert!(!gate.is_open());
        gate.set_engine_permits(true);
        assert!(gate.is_open());
        gate.set_session_permits(false);
        assert!(!gate.is_open());
    }

    #[test]
    fn io_needs_unlocked_and_active() {
        let ok = SessionState {
            lock: LockState::Unlocked,
            active: Some(true),
        };
        assert!(ok.permits_io());
        assert!(!SessionState { active: None, ..ok }.permits_io());
        assert!(
            !SessionState {
                active: Some(false),
                ..ok
            }
            .permits_io()
        );
        assert!(
            !SessionState {
                lock: LockState::Unknown,
                ..ok
            }
            .permits_io()
        );
        assert!(
            !SessionState {
                lock: LockState::Locked,
                ..ok
            }
            .permits_io()
        );
    }
}
