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

    /// Every native fact was read. An incomplete reading proves nothing: it can only be UNKNOWN.
    pub const fn is_complete(self) -> bool {
        self.console.is_some() && self.connected.is_some() && self.default_desktop.is_some()
    }

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

/// How long an incomplete native read waits before it is repeated.
pub const RETRY_INTERVAL: Duration = Duration::from_millis(50);
/// How many times an incomplete read is repeated before it is published as UNKNOWN. With
/// [`RETRY_INTERVAL`] the state that was proven last is held for at most 200 ms beyond the read
/// that failed.
pub const RETRY_LIMIT: u8 = 4;

/// What to do with a finished native read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Hand the reading to [`SessionModel::complete`], whatever it says.
    Publish,
    /// Publish nothing and read again after this delay. The state proven last stays published; no
    /// notification is held back, so a lock or a deactivation still closes the gate at once.
    Retry(Duration),
}

/// Decides whether a failed native query may be repeated before UNKNOWN is published (WP-W1.3).
///
/// A query can fail for a moment without the session having changed (the input desktop cannot be
/// opened while the display topology changes). Publishing UNKNOWN for that ends every projection
/// and every capture, so an incomplete read is repeated a few times first. This only delays the
/// publication of UNKNOWN. It never publishes anything else: a retry that is still incomplete ends
/// in UNKNOWN, and only a complete reading can say Unlocked.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadRetry {
    failed: u8,
}

impl ReadRetry {
    /// `proven` is the state the model published last. There is nothing to protect when it does
    /// not permit I/O already, so an incomplete read is published at once then.
    pub fn verdict(&mut self, reading: Reading, proven: SessionState) -> Verdict {
        if reading.is_complete() || !proven.permits_io() || self.failed >= RETRY_LIMIT {
            self.failed = 0;
            return Verdict::Publish;
        }
        self.failed += 1;
        Verdict::Retry(RETRY_INTERVAL)
    }
}

/// A gap over two seconds or disagreement with awake time needs fresh wake validation.
pub fn missed_sleep(monotonic: Duration, wall: Duration, awake: Duration) -> bool {
    let limit = Duration::from_secs(2);
    monotonic > limit || wall.abs_diff(awake) > limit
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: Reading = Reading {
        console: Some(true),
        connected: Some(true),
        default_desktop: Some(true),
    };
    const UNLOCKED: SessionState = SessionState {
        lock: LockState::Unlocked,
        active: Some(true),
    };

    fn partial() -> [Reading; 3] {
        [
            Reading {
                console: None,
                ..OPEN
            },
            Reading {
                connected: None,
                ..OPEN
            },
            Reading {
                default_desktop: None,
                ..OPEN
            },
        ]
    }

    #[test]
    fn session_retry_an_incomplete_reading_never_proves_anything() {
        assert!(OPEN.is_complete());
        assert_eq!(OPEN.state(), UNLOCKED);
        for reading in partial().into_iter().chain([Reading::UNKNOWN]) {
            assert!(!reading.is_complete());
            assert_eq!(reading.state(), UNKNOWN);
        }
    }

    #[test]
    fn session_retry_holds_a_proven_state_for_a_bounded_time_then_publishes() {
        let mut retry = ReadRetry::default();
        for _ in 0..RETRY_LIMIT {
            assert_eq!(
                retry.verdict(Reading::UNKNOWN, UNLOCKED),
                Verdict::Retry(RETRY_INTERVAL)
            );
        }
        assert_eq!(retry.verdict(Reading::UNKNOWN, UNLOCKED), Verdict::Publish);
        // The streak is spent: once UNKNOWN is published, the next failure publishes at once.
        assert_eq!(retry.verdict(Reading::UNKNOWN, UNKNOWN), Verdict::Publish);
        // The whole wait stays within the 200 ms the design allows.
        assert!(RETRY_INTERVAL * u32::from(RETRY_LIMIT) <= Duration::from_millis(200));
    }

    #[test]
    fn session_retry_covers_each_missing_fact_and_nothing_else() {
        for reading in partial() {
            let mut retry = ReadRetry::default();
            assert_eq!(
                retry.verdict(reading, UNLOCKED),
                Verdict::Retry(RETRY_INTERVAL)
            );
        }
        // A complete reading is always published, locked or not.
        let locked = Reading {
            default_desktop: Some(false),
            ..OPEN
        };
        let mut retry = ReadRetry::default();
        assert_eq!(retry.verdict(locked, UNLOCKED), Verdict::Publish);
        assert_eq!(retry.verdict(OPEN, UNLOCKED), Verdict::Publish);
    }

    #[test]
    fn session_retry_has_nothing_to_hold_unless_io_is_permitted() {
        let mut retry = ReadRetry::default();
        for proven in [
            UNKNOWN,
            SessionState {
                lock: LockState::Locked,
                active: Some(true),
            },
            SessionState {
                lock: LockState::Unlocked,
                active: Some(false),
            },
            SessionState {
                lock: LockState::Unlocked,
                active: None,
            },
        ] {
            assert_eq!(retry.verdict(Reading::UNKNOWN, proven), Verdict::Publish);
        }
    }

    #[test]
    fn session_retry_a_complete_reading_gives_the_next_failure_a_fresh_budget() {
        let mut retry = ReadRetry::default();
        for _ in 0..RETRY_LIMIT - 1 {
            assert!(matches!(
                retry.verdict(Reading::UNKNOWN, UNLOCKED),
                Verdict::Retry(_)
            ));
        }
        assert_eq!(retry.verdict(OPEN, UNLOCKED), Verdict::Publish);
        for _ in 0..RETRY_LIMIT {
            assert!(matches!(
                retry.verdict(Reading::UNKNOWN, UNLOCKED),
                Verdict::Retry(_)
            ));
        }
        assert_eq!(retry.verdict(Reading::UNKNOWN, UNLOCKED), Verdict::Publish);
    }

    #[test]
    fn session_retry_the_published_state_is_unchanged_while_a_read_is_repeated() {
        let mut model = SessionModel::default();
        let revision = model.begin_read();
        assert_eq!(model.complete(revision, OPEN).len(), 1);
        assert_eq!(model.state(), UNLOCKED);
        // A failed read is held back (nothing reaches the model), then a good one confirms the
        // state: no event, and never anything but the proven state in between.
        let mut retry = ReadRetry::default();
        assert!(matches!(
            retry.verdict(Reading::UNKNOWN, model.state()),
            Verdict::Retry(_)
        ));
        assert_eq!(model.state(), UNLOCKED);
        assert_eq!(retry.verdict(OPEN, model.state()), Verdict::Publish);
        assert!(model.complete(model.begin_read(), OPEN).is_empty());
        // Once the budget is spent the model publishes UNKNOWN, as it always did.
        let mut retry = ReadRetry::default();
        for _ in 0..RETRY_LIMIT {
            assert!(matches!(
                retry.verdict(Reading::UNKNOWN, model.state()),
                Verdict::Retry(_)
            ));
        }
        assert_eq!(
            retry.verdict(Reading::UNKNOWN, model.state()),
            Verdict::Publish
        );
        let events = model.complete(model.begin_read(), Reading::UNKNOWN);
        assert_eq!(events, vec![SessionEvent::State(UNKNOWN)]);
    }
}
