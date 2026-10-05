//! OS-free session observation and read-generation model (WP-W1.3).

use std::time::Duration;

use crosspane_platform::{LockState, SessionEvent, SessionState};

/// The only safe answer when observation is unavailable.
pub const UNKNOWN: SessionState = SessionState {
    lock: LockState::Unknown,
    active: None,
};

/// Independently validated native facts; missing facts are never permission.
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    /// Process session equals the non-sentinel active console session.
    pub console: Option<bool>,
    /// WTS reports the session's signed-in, actively connected state.
    pub connected: Option<bool>,
    /// The currently receiving input desktop has the exact default desktop name.
    pub default_desktop: Option<bool>,
}

impl Reading {
    /// An incomplete or failed native read.
    pub const UNKNOWN: Self = Self {
        console: None,
        connected: None,
        default_desktop: None,
    };

    fn state(self) -> SessionState {
        match (self.console, self.connected, self.default_desktop) {
            (Some(console), Some(connected), Some(default)) => {
                let active = console && connected;
                SessionState {
                    active: Some(active),
                    lock: if !default {
                        LockState::Locked
                    } else if active {
                        LockState::Unlocked
                    } else {
                        LockState::Unknown
                    },
                }
            }
            _ => UNKNOWN,
        }
    }
}

/// Notifications invalidate observations; none grants permission.
#[derive(Clone, Copy, Debug)]
pub enum Signal {
    /// Session lock observed before desktop revalidation.
    Lock,
    /// Unlock announced; still requires a later read.
    Unlock,
    /// Console/remote disconnection, logoff or deactivation.
    Inactive,
    /// Connection/activation change requiring a new read.
    Refresh,
    /// Announced system suspension.
    Sleep,
    /// Announced resume.
    Wake,
    /// Polling backstop; never overrides an already announced suspension.
    MissedSleep,
    /// Observer died or can no longer provide trustworthy observation.
    Lost,
}

/// Pure state behind the gate; adapters apply its state before delivering events.
#[derive(Debug)]
pub struct SessionModel {
    state: SessionState,
    revision: u64,
    sleeping: bool,
    lost: bool,
    lock_requested: bool,
    lock_seen: bool,
    force_state: bool,
}

impl Default for SessionModel {
    fn default() -> Self {
        Self {
            state: UNKNOWN,
            revision: 0,
            sleeping: false,
            lost: false,
            lock_requested: false,
            lock_seen: false,
            force_state: false,
        }
    }
}

impl SessionModel {
    /// Latest proven state.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Capture before beginning an OS read, never after it.
    pub fn begin_read(&self) -> u64 {
        self.revision
    }

    /// Publish a completed native observation.
    pub fn complete(&mut self, revision: u64, reading: Reading) -> Vec<SessionEvent> {
        if self.lost || revision != self.revision {
            return Vec::new();
        }
        let mut state = reading.state();
        if state.lock == LockState::Locked {
            self.lock_seen = true;
        } else if state.lock == LockState::Unlocked && self.lock_seen {
            self.lock_requested = false;
            self.lock_seen = false;
        }
        if self.sleeping {
            state = UNKNOWN;
        } else if self.lock_requested && state.lock == LockState::Unlocked {
            state.lock = LockState::Locked;
        }
        let force = std::mem::take(&mut self.force_state);
        if state == self.state && !force {
            return Vec::new();
        }
        self.state = state;
        vec![SessionEvent::State(state)]
    }

    /// Invalidate at the notification boundary.
    pub fn signal(&mut self, signal: Signal) -> Vec<SessionEvent> {
        if self.lost {
            return Vec::new();
        }
        let Some(revision) = self.revision.checked_add(1) else {
            self.lost = true;
            self.state = UNKNOWN;
            return vec![SessionEvent::State(UNKNOWN)];
        };
        self.revision = revision;
        self.force_state = true;
        let mut events = Vec::new();
        self.state = match signal {
            Signal::Lock => {
                self.lock_requested = true;
                SessionState {
                    lock: LockState::Locked,
                    active: self.state.active,
                }
            }
            Signal::Unlock => {
                self.lock_requested = false;
                self.lock_seen = false;
                UNKNOWN
            }
            Signal::Inactive => SessionState {
                lock: if self.state.lock == LockState::Locked {
                    LockState::Locked
                } else {
                    LockState::Unknown
                },
                active: Some(false),
            },
            Signal::Sleep => {
                self.sleeping = true;
                events.push(SessionEvent::WillSleep);
                UNKNOWN
            }
            Signal::Wake => {
                self.sleeping = false;
                events.push(SessionEvent::Woke);
                UNKNOWN
            }
            Signal::MissedSleep => {
                // Scheduler/clock gaps are not proof of resume after an announced sleep.
                if !self.sleeping {
                    events.push(SessionEvent::Woke);
                }
                UNKNOWN
            }
            Signal::Lost => {
                self.lost = true;
                UNKNOWN
            }
            Signal::Refresh => UNKNOWN,
        };
        events.push(SessionEvent::State(self.state));
        events
    }
}

/// A gap over two seconds or disagreement with awake time needs fresh wake validation.
pub fn missed_sleep(monotonic: Duration, wall: Duration, awake: Duration) -> bool {
    let limit = Duration::from_secs(2);
    monotonic > limit || wall.abs_diff(awake) > limit
}
