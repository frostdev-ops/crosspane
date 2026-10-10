//! The session lifecycle as a pure state machine.
//!
//! The worker feeds it [`Event`]s and reports the statuses it returns, in order. It owns every
//! epoch and retry decision; it never touches D-Bus, threads or time, so the whole policy is
//! testable here.
//!
//! - Epochs are numbered 1, 2, … by successful starts ([`Event::Granted`]) and never repeat.
//! - A denied start is `Denied` and stays so until [`Event::Restart`].
//! - When the portal closes the active session ([`Event::Revoked`]: the user or the desktop ended
//!   remote control, e.g. the Stop button of GNOME's indicator), the machine reports `Closed` and
//!   stays there until [`Event::Restart`]. It never re-arms on its own: that would override the
//!   user's explicit stop (amended 2026-10-09, lead).
//! - When the portal itself goes away ([`Event::PortalLost`]: its bus name lost its owner or changed
//!   hands), the machine reports `Closed` and asks for one silent retry. If the retry doesn't start
//!   a session, the status stays `Closed`. The retry budget is renewed only by an explicit restart,
//!   so a portal that keeps crashing is not re-armed forever.
//! - [`Event::Close`] is final.

use super::SessionStatus;

/// What the worker should be doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// A start attempt is in flight. `silent` marks the automatic retry after a lost portal: its
    /// status stays the `Closed` that preceded it, even if it has to show the consent dialog.
    Starting { silent: bool },
    /// The session of this epoch is active.
    Active { epoch: u64 },
    /// Nothing to do until [`Event::Restart`] or [`Event::Close`]. The status is final.
    Idle,
    /// Closed for good; the worker exits.
    Finished,
}

/// Why a start attempt did not produce a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Failure {
    /// The user denied or cancelled, or the desktop granted less than keyboard and pointer.
    Denied,
    /// No usable portal: absent, too old, or failing.
    Unavailable,
}

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Event {
    /// `restart()` was called.
    Restart,
    /// `close()` was called (or the handle was dropped).
    Close,
    /// The attempt in flight produced an active, published session: the next epoch begins.
    Granted,
    /// The attempt in flight ended without a session.
    Failed(Failure),
    /// The portal reported the active session `Closed`: the user or the desktop ended it. Final
    /// until `Restart`; never retried.
    Revoked,
    /// The portal's bus name lost its owner or changed hands while the session was active. Retried
    /// once.
    PortalLost,
}

/// The status a session in `status` has once it is closed from outside: an active epoch is closed,
/// a start in flight is closed with `last_epoch` (0 if no session ever started), and final
/// statuses stay as they are.
pub(super) fn closed_status(status: SessionStatus, last_epoch: u64) -> SessionStatus {
    match status {
        SessionStatus::Active { epoch } => SessionStatus::Closed { epoch },
        SessionStatus::Pending => SessionStatus::Closed { epoch: last_epoch },
        other => other,
    }
}

/// The state machine. `step` is the only mutator.
#[derive(Debug)]
pub(super) struct Lifecycle {
    phase: Phase,
    /// The last epoch handed out; 0 before the first.
    epoch: u64,
    /// The last status returned by `step` (initially `Pending`, reported by the worker itself).
    status: SessionStatus,
    /// Whether a lost portal may be retried silently once.
    retry_left: bool,
}

impl Lifecycle {
    /// A machine that has just started its first attempt, with `Pending` as its status.
    pub(super) fn new() -> Lifecycle {
        Lifecycle {
            phase: Phase::Starting { silent: false },
            epoch: 0,
            status: SessionStatus::Pending,
            retry_left: true,
        }
    }

    pub(super) fn phase(&self) -> Phase {
        self.phase
    }

    /// The epoch the next [`Event::Granted`] will begin.
    pub(super) fn next_epoch(&self) -> u64 {
        self.epoch + 1
    }

    /// Apply `event` and return the status changes to report, in order. Events that make no sense
    /// in the current phase are ignored.
    pub(super) fn step(&mut self, event: Event) -> Vec<SessionStatus> {
        let mut out = Vec::new();
        match (self.phase, event) {
            (Phase::Finished, _) => {}

            (Phase::Starting { .. }, Event::Granted) => {
                self.epoch += 1;
                self.phase = Phase::Active { epoch: self.epoch };
                self.report(&mut out, SessionStatus::Active { epoch: self.epoch });
            }
            (Phase::Starting { silent }, Event::Failed(failure)) => {
                self.phase = Phase::Idle;
                if !silent {
                    self.report(
                        &mut out,
                        match failure {
                            Failure::Denied => SessionStatus::Denied,
                            Failure::Unavailable => SessionStatus::Unavailable,
                        },
                    );
                }
            }
            (Phase::Active { epoch }, Event::Revoked) => {
                self.report(&mut out, SessionStatus::Closed { epoch });
                self.phase = Phase::Idle;
            }
            (Phase::Active { epoch }, Event::PortalLost) => {
                self.report(&mut out, SessionStatus::Closed { epoch });
                self.phase = if self.retry_left {
                    self.retry_left = false;
                    Phase::Starting { silent: true }
                } else {
                    Phase::Idle
                };
            }

            (Phase::Active { epoch }, Event::Restart) => {
                self.report(&mut out, SessionStatus::Closed { epoch });
                self.begin_explicit(&mut out);
            }
            (Phase::Starting { .. } | Phase::Idle, Event::Restart) => {
                self.begin_explicit(&mut out);
            }

            (Phase::Active { .. } | Phase::Starting { .. }, Event::Close) => {
                let closed = closed_status(self.status, self.epoch);
                self.report(&mut out, closed);
                self.phase = Phase::Finished;
            }
            (Phase::Idle, Event::Close) => self.phase = Phase::Finished,

            // `Granted`/`Failed` outside a start, `Revoked`/`PortalLost` outside an active epoch.
            _ => {}
        }
        out
    }

    /// An explicit (re)start: a fresh attempt with a fresh retry budget.
    fn begin_explicit(&mut self, out: &mut Vec<SessionStatus>) {
        self.retry_left = true;
        self.phase = Phase::Starting { silent: false };
        self.report(out, SessionStatus::Pending);
    }

    /// Record `status` and queue it for reporting, unless it is the status already reported.
    fn report(&mut self, out: &mut Vec<SessionStatus>, status: SessionStatus) {
        if self.status != status {
            self.status = status;
            out.push(status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionStatus::{Active, Closed, Denied, Pending, Unavailable};

    fn active(lc: &mut Lifecycle) -> Vec<SessionStatus> {
        lc.step(Event::Granted)
    }

    #[test]
    fn first_grant_is_epoch_one() {
        let mut lc = Lifecycle::new();
        assert_eq!(lc.phase(), Phase::Starting { silent: false });
        assert_eq!(lc.next_epoch(), 1);
        assert_eq!(active(&mut lc), vec![Active { epoch: 1 }]);
        assert_eq!(lc.phase(), Phase::Active { epoch: 1 });
        assert_eq!(lc.next_epoch(), 2);
    }

    #[test]
    fn denied_is_final_until_restart() {
        let mut lc = Lifecycle::new();
        assert_eq!(lc.step(Event::Failed(Failure::Denied)), vec![Denied]);
        assert_eq!(lc.phase(), Phase::Idle);
        // Nothing but Restart or Close moves it.
        assert!(lc.step(Event::Revoked).is_empty());
        assert!(lc.step(Event::PortalLost).is_empty());
        assert!(lc.step(Event::Granted).is_empty());
        assert_eq!(lc.phase(), Phase::Idle);
        assert_eq!(lc.step(Event::Restart), vec![Pending]);
        assert_eq!(lc.phase(), Phase::Starting { silent: false });
        assert_eq!(active(&mut lc), vec![Active { epoch: 1 }]);
    }

    #[test]
    fn unavailable_is_reported_and_restartable() {
        let mut lc = Lifecycle::new();
        assert_eq!(
            lc.step(Event::Failed(Failure::Unavailable)),
            vec![Unavailable]
        );
        assert_eq!(lc.step(Event::Restart), vec![Pending]);
    }

    #[test]
    fn a_revoked_session_stays_closed_and_is_never_retried() {
        let mut lc = Lifecycle::new();
        active(&mut lc);
        // The user pressed Stop: closed, and the machine waits for an explicit restart.
        assert_eq!(lc.step(Event::Revoked), vec![Closed { epoch: 1 }]);
        assert_eq!(lc.phase(), Phase::Idle);
        // Nothing but Restart or Close moves it.
        assert!(lc.step(Event::Revoked).is_empty());
        assert!(lc.step(Event::PortalLost).is_empty());
        assert!(lc.step(Event::Granted).is_empty());
        assert_eq!(lc.phase(), Phase::Idle);
        assert_eq!(lc.step(Event::Restart), vec![Pending]);
        assert_eq!(active(&mut lc), vec![Active { epoch: 2 }]);
    }

    #[test]
    fn a_revoke_with_the_retry_budget_unspent_still_does_not_retry() {
        // First start, budget untouched: the revoke must not use it.
        let mut lc = Lifecycle::new();
        active(&mut lc);
        lc.step(Event::Revoked);
        assert_eq!(lc.phase(), Phase::Idle);

        // The epoch a portal-loss retry began is revoked: also final.
        let mut lc = Lifecycle::new();
        active(&mut lc);
        lc.step(Event::PortalLost);
        assert_eq!(active(&mut lc), vec![Active { epoch: 2 }]);
        assert_eq!(lc.step(Event::Revoked), vec![Closed { epoch: 2 }]);
        assert_eq!(lc.phase(), Phase::Idle);
    }

    #[test]
    fn a_lost_portal_retries_silently_once() {
        let mut lc = Lifecycle::new();
        active(&mut lc);
        assert_eq!(lc.step(Event::PortalLost), vec![Closed { epoch: 1 }]);
        assert_eq!(lc.phase(), Phase::Starting { silent: true });
        // A successful retry is the next epoch.
        assert_eq!(active(&mut lc), vec![Active { epoch: 2 }]);
        // The retry budget is spent: the second loss stays closed.
        assert_eq!(lc.step(Event::PortalLost), vec![Closed { epoch: 2 }]);
        assert_eq!(lc.phase(), Phase::Idle);
    }

    #[test]
    fn failed_silent_retry_stays_closed_without_a_new_report() {
        for failure in [Failure::Denied, Failure::Unavailable] {
            let mut lc = Lifecycle::new();
            active(&mut lc);
            lc.step(Event::PortalLost);
            assert_eq!(lc.phase(), Phase::Starting { silent: true });
            assert!(lc.step(Event::Failed(failure)).is_empty());
            assert_eq!(lc.phase(), Phase::Idle);
            // Still closed: the only way out is an explicit restart.
            assert!(lc.step(Event::Revoked).is_empty());
            assert!(lc.step(Event::PortalLost).is_empty());
            assert_eq!(lc.step(Event::Restart), vec![Pending]);
        }
    }

    #[test]
    fn restart_renews_the_retry_budget() {
        let mut lc = Lifecycle::new();
        active(&mut lc);
        lc.step(Event::PortalLost);
        active(&mut lc);
        lc.step(Event::PortalLost);
        assert_eq!(lc.phase(), Phase::Idle);
        assert_eq!(lc.step(Event::Restart), vec![Pending]);
        assert_eq!(active(&mut lc), vec![Active { epoch: 3 }]);
        assert_eq!(lc.step(Event::PortalLost), vec![Closed { epoch: 3 }]);
        assert_eq!(lc.phase(), Phase::Starting { silent: true });
    }

    #[test]
    fn restart_from_active_closes_the_epoch_then_goes_pending() {
        let mut lc = Lifecycle::new();
        active(&mut lc);
        assert_eq!(lc.step(Event::Restart), vec![Closed { epoch: 1 }, Pending]);
        assert_eq!(lc.phase(), Phase::Starting { silent: false });
        assert_eq!(active(&mut lc), vec![Active { epoch: 2 }]);
    }

    #[test]
    fn restart_while_pending_reports_nothing_new() {
        let mut lc = Lifecycle::new();
        assert!(lc.step(Event::Restart).is_empty());
        assert_eq!(lc.phase(), Phase::Starting { silent: false });
        assert_eq!(lc.next_epoch(), 1);
    }

    #[test]
    fn restart_during_the_silent_retry_goes_pending() {
        let mut lc = Lifecycle::new();
        active(&mut lc);
        lc.step(Event::PortalLost);
        assert_eq!(lc.step(Event::Restart), vec![Pending]);
        assert_eq!(lc.phase(), Phase::Starting { silent: false });
    }

    #[test]
    fn close_while_active_is_final() {
        let mut lc = Lifecycle::new();
        active(&mut lc);
        assert_eq!(lc.step(Event::Close), vec![Closed { epoch: 1 }]);
        assert_eq!(lc.phase(), Phase::Finished);
        // Everything after is ignored; no epoch is ever handed out again.
        for event in [
            Event::Restart,
            Event::Close,
            Event::Granted,
            Event::Revoked,
            Event::PortalLost,
            Event::Failed(Failure::Denied),
        ] {
            assert!(lc.step(event).is_empty());
            assert_eq!(lc.phase(), Phase::Finished);
        }
    }

    #[test]
    fn close_while_pending_reports_the_last_epoch() {
        let mut lc = Lifecycle::new();
        assert_eq!(lc.step(Event::Close), vec![Closed { epoch: 0 }]);
        assert_eq!(lc.phase(), Phase::Finished);

        let mut lc = Lifecycle::new();
        active(&mut lc);
        lc.step(Event::Restart);
        assert_eq!(lc.step(Event::Close), vec![Closed { epoch: 1 }]);
    }

    #[test]
    fn close_during_the_silent_retry_keeps_the_closed_status() {
        let mut lc = Lifecycle::new();
        active(&mut lc);
        lc.step(Event::PortalLost);
        assert!(lc.step(Event::Close).is_empty());
        assert_eq!(lc.phase(), Phase::Finished);
    }

    #[test]
    fn close_from_a_final_status_keeps_it() {
        let mut lc = Lifecycle::new();
        lc.step(Event::Failed(Failure::Denied));
        assert!(lc.step(Event::Close).is_empty());
        assert_eq!(lc.phase(), Phase::Finished);
    }

    #[test]
    fn a_stale_grant_after_close_never_becomes_active() {
        let mut lc = Lifecycle::new();
        lc.step(Event::Close);
        assert!(lc.step(Event::Granted).is_empty());
        assert_eq!(lc.next_epoch(), 1);
    }

    #[test]
    fn epochs_increase_strictly_across_restarts_and_retries() {
        let mut lc = Lifecycle::new();
        let mut seen = Vec::new();
        for round in 0..4 {
            for status in active(&mut lc) {
                if let Active { epoch } = status {
                    seen.push(epoch);
                }
            }
            if round % 2 == 0 {
                lc.step(Event::PortalLost);
            } else {
                lc.step(Event::Restart);
            }
        }
        assert_eq!(seen, vec![1, 2, 3, 4]);
    }

    #[test]
    fn closed_status_of_each_status() {
        assert_eq!(closed_status(Active { epoch: 4 }, 4), Closed { epoch: 4 });
        assert_eq!(closed_status(Pending, 0), Closed { epoch: 0 });
        assert_eq!(closed_status(Pending, 3), Closed { epoch: 3 });
        assert_eq!(closed_status(Denied, 3), Denied);
        assert_eq!(closed_status(Unavailable, 3), Unavailable);
        assert_eq!(closed_status(Closed { epoch: 2 }, 3), Closed { epoch: 2 });
    }
}
