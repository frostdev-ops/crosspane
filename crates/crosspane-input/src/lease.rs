//! Target input leases and controller heartbeat/acknowledgement deadlines.

use core::time::Duration;
use std::collections::{BTreeSet, VecDeque};

use crosspane_types::time::MonoTime;

use crate::Held;
use crate::journal::{Journal, JournalError};
use crate::timing::{
    ACK_TIMEOUT_MIN, ACK_TIMEOUT_RTT_FACTOR, HEARTBEAT_HELD, HEARTBEAT_IDLE, LEASE_TIMEOUT,
};

/// What the target must do to its injectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Press(Held),
    Release(Held),
}

/// The target side of invariant 2 for one controller.
#[derive(Debug)]
pub struct TargetLedger<J: Journal> {
    journal: J,
    held: BTreeSet<Held>,
    pending_releases: BTreeSet<Held>,
    lease_start: Option<MonoTime>,
}

impl<J: Journal> TargetLedger<J> {
    /// Open the ledger. Also returns the items a previous process left held, from the journal; the
    /// caller releases them (`recover_keys`/`recover_buttons`) and then calls `recovered`.
    pub fn open(journal: J) -> Result<(Self, Vec<Held>), JournalError> {
        let recovery = journal.held()?;
        let ledger = Self {
            journal,
            held: BTreeSet::new(),
            pending_releases: recovery.iter().copied().collect(),
            lease_start: None,
        };
        Ok((ledger, recovery))
    }

    /// The recovery releases succeeded: clear them from the journal.
    pub fn recovered(&mut self, items: &[Held]) -> Result<(), JournalError> {
        self.confirm_released(items)
    }

    /// A down or up from the controller.
    /// - A down is journaled *before* `Press` is returned.
    /// - A duplicate down, or an up for something not held, returns `None`.
    /// - An up returns `Release`; its journal record is written by `confirm_released`.
    /// - The first message received starts the lease clock.
    pub fn on_input(
        &mut self,
        item: Held,
        down: bool,
        now: MonoTime,
    ) -> Result<Option<Action>, JournalError> {
        self.lease_start.get_or_insert(now);
        if down {
            if self.held.contains(&item) {
                return Ok(None);
            }
            self.journal.record_down(item)?;
            self.held.insert(item);
            // A later down still needs recovery even if an earlier release was unconfirmed.
            self.pending_releases.remove(&item);
            Ok(Some(Action::Press(item)))
        } else if self.held.remove(&item) {
            self.pending_releases.insert(item);
            Ok(Some(Action::Release(item)))
        } else {
            Ok(None)
        }
    }

    /// A heartbeat listing everything the controller believes is held here: release anything held
    /// here that isn't listed (in sorted order), and refresh the lease.
    pub fn on_heartbeat(&mut self, listed: &[Held], now: MonoTime) -> Vec<Action> {
        self.lease_start = Some(now);
        let listed: BTreeSet<_> = listed.iter().copied().collect();
        let released: Vec<_> = self.held.difference(&listed).copied().collect();
        for item in &released {
            self.held.remove(item);
            self.pending_releases.insert(*item);
        }
        released.into_iter().map(Action::Release).collect()
    }

    /// If anything is held and the lease (last heartbeat, or the first message if there has been no
    /// heartbeat) is older than `LEASE_TIMEOUT`, release everything (sorted).
    pub fn on_tick(&mut self, now: MonoTime) -> Vec<Action> {
        if self
            .lease_start
            .is_some_and(|start| now.saturating_duration_since(start) > LEASE_TIMEOUT)
        {
            self.release_all()
        } else {
            Vec::new()
        }
    }

    /// Release everything (session end, lock, panic), sorted.
    pub fn release_all(&mut self) -> Vec<Action> {
        let released = std::mem::take(&mut self.held);
        self.pending_releases.extend(&released);
        released.into_iter().map(Action::Release).collect()
    }

    /// The injector returned `Ok` for these releases: record them up in the journal. Releases not
    /// confirmed stay journaled.
    pub fn confirm_released(&mut self, items: &[Held]) -> Result<(), JournalError> {
        for item in items {
            if self.pending_releases.contains(item) {
                self.journal.record_up(*item)?;
                self.pending_releases.remove(item);
            }
        }
        Ok(())
    }

    /// Everything currently held through this ledger, sorted.
    pub fn held(&self) -> Vec<Held> {
        self.held.iter().copied().collect()
    }

    /// When `on_tick` must next run: lease start + `LEASE_TIMEOUT` while anything is held.
    pub fn next_deadline(&self) -> Option<MonoTime> {
        if self.held.is_empty() {
            None
        } else {
            self.lease_start
                .map(|start| start.saturating_add(LEASE_TIMEOUT))
        }
    }
}

/// The controller side of invariants 2–3 for one session.
#[derive(Clone, Debug)]
pub struct ControllerLease {
    started: MonoTime,
    last_heartbeat: Option<MonoTime>,
    last_sent: Option<u32>,
    unacked: VecDeque<(u32, MonoTime)>,
}

impl ControllerLease {
    pub fn new(now: MonoTime) -> Self {
        Self {
            started: now,
            last_heartbeat: None,
            last_sent: None,
            unacked: VecDeque::new(),
        }
    }

    /// A message with sequence number `seq` was sent. `seq` values increase.
    pub fn sent(&mut self, seq: u32, now: MonoTime) {
        if self.last_sent.is_none_or(|last| seq > last) {
            self.unacked.push_back((seq, now));
            self.last_sent = Some(seq);
        }
    }

    /// The target acknowledged everything up to and including `seq`. Acks for unsent or already
    /// acknowledged sequence numbers are ignored.
    pub fn acked(&mut self, seq: u32, _now: MonoTime) {
        if let Some(index) = self.unacked.iter().position(|&(sent, _)| sent == seq) {
            self.unacked.drain(..=index);
        }
    }

    /// A heartbeat was sent now. Also call `sent` with its `seq`.
    pub fn heartbeat_sent(&mut self, now: MonoTime) {
        self.last_heartbeat = Some(now);
    }

    /// When the next heartbeat is due: last heartbeat + `HEARTBEAT_HELD` if anything is held,
    /// otherwise + `HEARTBEAT_IDLE`. Before the first heartbeat: `new`'s time.
    pub fn next_heartbeat(&self, anything_held: bool) -> MonoTime {
        self.last_heartbeat.map_or(self.started, |last| {
            last.saturating_add(if anything_held {
                HEARTBEAT_HELD
            } else {
                HEARTBEAT_IDLE
            })
        })
    }

    /// True when the oldest unacknowledged message was sent more than
    /// `max(ACK_TIMEOUT_MIN, ACK_TIMEOUT_RTT_FACTOR × rtt)` before `now` (`rtt` `None` → the minimum).
    pub fn lost(&self, now: MonoTime, rtt: Option<Duration>) -> bool {
        let timeout = rtt.map_or(ACK_TIMEOUT_MIN, |rtt| {
            ACK_TIMEOUT_MIN.max(
                rtt.checked_mul(ACK_TIMEOUT_RTT_FACTOR)
                    .unwrap_or(Duration::MAX),
            )
        });
        self.unacked
            .front()
            .is_some_and(|&(_, sent)| now.saturating_duration_since(sent) > timeout)
    }
}
