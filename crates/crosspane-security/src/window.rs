//! Time and attempt limits for an initiating node's pairing window (04 §2).

use core::time::Duration;

use crosspane_types::time::MonoTime;

pub const PAIRING_WINDOW: Duration = Duration::from_secs(120);
pub const MAX_ATTEMPTS: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WindowError {
    #[error("the pairing window has closed")]
    Expired,
    #[error("too many pairing attempts")]
    TooManyAttempts,
}

/// One pairing window on the initiating node (04 §2: ≤ 120 s, ≤ 3 attempts; one window at a time
/// is enforced by the agent).
#[derive(Clone, Debug)]
pub struct PairingWindow {
    opened_at: MonoTime,
    attempts: u8,
}

impl PairingWindow {
    pub fn open(now: MonoTime) -> PairingWindow {
        Self {
            opened_at: now,
            attempts: 0,
        }
    }

    pub fn is_open(&self, now: MonoTime) -> bool {
        !self.expired(now) && self.attempts < MAX_ATTEMPTS
    }

    /// Admit one more attempt; fails once expired or after `MAX_ATTEMPTS` attempts.
    pub fn admit(&mut self, now: MonoTime) -> Result<u8, WindowError> {
        if self.expired(now) {
            return Err(WindowError::Expired);
        }
        if self.attempts >= MAX_ATTEMPTS {
            return Err(WindowError::TooManyAttempts);
        }
        self.attempts += 1;
        Ok(self.attempts)
    }

    fn expired(&self, now: MonoTime) -> bool {
        // Elapsed time avoids an overflowing deadline near MonoTime's maximum.
        now.saturating_duration_since(self.opened_at) >= PAIRING_WINDOW
    }
}
