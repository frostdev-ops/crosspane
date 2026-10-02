//! Per-projection leases over a shared journal of the union of held items.
//!
//! Each scoped journal keeps its own ownership set. An up removes the underlying record only
//! after the last owner confirms release. Owner zero is startup recovery. Ended projections keep
//! only their release bookkeeping until confirmation; they cannot receive any further input.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crosspane_input::Held;
use crosspane_input::journal::{Journal, JournalError};
use crosspane_input::lease::{Action, TargetLedger};
use crosspane_input::timing::LEASE_TIMEOUT;
use crosspane_protocol::projection::ProjInput;
use crosspane_types::id::ProjectionId;
use crosspane_types::time::MonoTime;

use crate::io::{InjectCmd, InjectId, Output};

const RETRY: Duration = Duration::from_millis(50);
const TARGET_QUEUE_LIMIT: usize = 64;
const TARGET_ACK_TIMEOUT: Duration = Duration::from_millis(500);

struct Shared {
    journal: Box<dyn Journal>,
    owners: BTreeMap<ProjectionId, BTreeSet<Held>>,
}

#[derive(Clone)]
struct Scoped {
    shared: Arc<Mutex<Shared>>,
    owner: ProjectionId,
}

impl Scoped {
    fn lock(&self) -> Result<MutexGuard<'_, Shared>, JournalError> {
        self.shared
            .lock()
            .map_err(|_| io::Error::other("E2 journal mutex poisoned").into())
    }
}

impl Journal for Scoped {
    fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
        let mut shared = self.lock()?;
        let result = shared.journal.record_down(item);
        // Even a failed write may have reached the journal: retain it for conservative cleanup.
        shared.owners.entry(self.owner).or_default().insert(item);
        result
    }

    fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
        let mut shared = self.lock()?;
        let another_owner = shared
            .owners
            .iter()
            .any(|(&owner, items)| owner != self.owner && items.contains(&item));
        if !another_owner {
            shared.journal.record_up(item)?;
        }
        if let Some(items) = shared.owners.get_mut(&self.owner) {
            items.remove(&item);
            if items.is_empty() {
                shared.owners.remove(&self.owner);
            }
        }
        Ok(())
    }

    fn held(&self) -> Result<Vec<Held>, JournalError> {
        Ok(self
            .lock()?
            .owners
            .get(&self.owner)
            .map(|items| items.iter().copied().collect())
            .unwrap_or_default())
    }
}

struct Lease {
    ledger: TargetLedger<Scoped>,
    generations: BTreeMap<Held, u64>,
    unconfirmed: BTreeMap<Held, u64>,
    retry: Option<MonoTime>,
    retired: bool,
    // Unlike TargetLedger::next_deadline, this also survives an empty held set.
    expires: Option<MonoTime>,
}

impl Lease {
    fn retry_actions(&mut self, now: MonoTime, mut actions: Vec<Action>) -> Vec<Action> {
        if self.retry.is_some_and(|deadline| deadline <= now) {
            let already: BTreeSet<_> = actions
                .iter()
                .filter_map(|action| match action {
                    Action::Release(item) => Some(*item),
                    _ => None,
                })
                .collect();
            actions.extend(
                self.unconfirmed
                    .keys()
                    .filter(|item| !already.contains(item))
                    .copied()
                    .map(Action::Release),
            );
            self.retry = now.checked_add(RETRY);
        }
        actions
    }
}

struct Pending {
    owner: ProjectionId,
    items: Vec<(Held, u64)>,
}

pub(super) struct Targeting {
    pub id: InjectId,
    pub owner: ProjectionId,
    pub input: ProjInput,
    pub deadline: MonoTime,
}

pub(super) struct Ledgers {
    shared: Arc<Mutex<Shared>>,
    leases: BTreeMap<ProjectionId, Lease>,
    pending: BTreeMap<InjectId, Pending>,
    // Kept with the lease so home drains, retirement and lease expiry also cancel unissued input.
    targeting: Option<Targeting>,
    queued: VecDeque<(ProjectionId, ProjInput)>,
    // E1 allocates upward from 1. E2 allocates downward from the other end, so the agent can
    // broadcast InjectDone to both roles without confusing their outstanding requests.
    next_id: u64,
}

impl Ledgers {
    pub fn new(journal: Box<dyn Journal>, out: &mut Vec<Output>) -> Result<Self, JournalError> {
        let recovery = journal.held()?;
        let shared = Arc::new(Mutex::new(Shared {
            journal,
            owners: [(ProjectionId(0), recovery.iter().copied().collect())].into(),
        }));
        let mut this = Self {
            shared,
            leases: BTreeMap::new(),
            pending: BTreeMap::new(),
            targeting: None,
            queued: VecDeque::new(),
            next_id: u64::MAX,
        };
        this.open(ProjectionId(0))?;
        if !recovery.is_empty() {
            let (keys, buttons) = split(&recovery);
            let items = recovery.into_iter().map(|item| (item, 0)).collect();
            if let Some(lease) = this.leases.get_mut(&ProjectionId(0)) {
                lease.unconfirmed = items;
            }
            this.submit_recovery(keys, buttons, out);
        }
        Ok(this)
    }

    pub fn recovery_done(&self) -> bool {
        self.leases
            .get(&ProjectionId(0))
            .is_none_or(|lease| lease.unconfirmed.is_empty())
    }

    /// WP-2.43 §2.3 step 1: nothing is held through any lease, no release is unconfirmed, no retry
    /// is scheduled, no release is awaiting its `InjectDone`, and startup recovery is done.
    pub fn settled(&self) -> bool {
        self.recovery_done()
            && self.pending.is_empty()
            && self.leases.values().all(|lease| {
                lease.unconfirmed.is_empty()
                    && lease.retry.is_none()
                    && lease.ledger.held().is_empty()
            })
    }

    pub fn open(&mut self, owner: ProjectionId) -> Result<(), JournalError> {
        if let Some(lease) = self.leases.get_mut(&owner) {
            // Resume the empty ledger without losing unconfirmed releases, retry deadlines or
            // generations. Old InjectDone callbacks must never clear a new press's journal.
            lease.retired = false;
            return Ok(());
        }
        let (ledger, _) = TargetLedger::open(Scoped {
            shared: self.shared.clone(),
            owner,
        })?;
        self.leases.insert(
            owner,
            Lease {
                ledger,
                generations: BTreeMap::new(),
                unconfirmed: BTreeMap::new(),
                retry: None,
                retired: owner == ProjectionId(0),
                expires: None,
            },
        );
        Ok(())
    }

    pub fn inject(&mut self, cmd: InjectCmd, out: &mut Vec<Output>) {
        let id = self.allocate();
        out.push(Output::Inject { id, cmd });
    }

    pub fn target(
        &mut self,
        owner: ProjectionId,
        cmd: InjectCmd,
        input: ProjInput,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let id = self.allocate();
        self.targeting = Some(Targeting {
            id,
            owner,
            input,
            deadline: now.saturating_add(TARGET_ACK_TIMEOUT),
        });
        out.push(Output::Inject { id, cmd });
    }

    /// Serialize every projection over the shared pointer until its targeting action is emitted.
    pub fn queue_targeted(&mut self, owner: ProjectionId, input: &ProjInput) -> Result<bool, ()> {
        if self.targeting.is_none() {
            return Ok(false);
        }
        if self.queued.len() == TARGET_QUEUE_LIMIT {
            return Err(());
        }
        self.queued.push_back((owner, input.clone()));
        Ok(true)
    }

    pub fn targeted(&mut self, id: InjectId) -> Option<Targeting> {
        if self
            .targeting
            .as_ref()
            .is_some_and(|targeting| targeting.id == id)
        {
            self.targeting.take()
        } else {
            None
        }
    }

    pub fn next_targeted_input(&mut self) -> Option<(ProjectionId, ProjInput)> {
        if self.targeting.is_none() {
            self.queued.pop_front()
        } else {
            None
        }
    }

    pub fn targeting_expired(&self, now: MonoTime) -> Option<ProjectionId> {
        self.targeting
            .as_ref()
            .filter(|targeting| targeting.deadline <= now)
            .map(|targeting| targeting.owner)
    }

    fn cancel_targeting(&mut self, owner: ProjectionId) {
        if self
            .targeting
            .as_ref()
            .is_some_and(|targeting| targeting.owner == owner)
        {
            self.targeting = None;
        }
        self.queued.retain(|(projection, _)| *projection != owner);
    }

    pub fn holds(&self, owner: ProjectionId, item: Held) -> bool {
        self.leases
            .get(&owner)
            .is_some_and(|lease| lease.ledger.held().contains(&item))
    }

    fn allocate(&mut self) -> InjectId {
        let id = InjectId(self.next_id);
        self.next_id = self.next_id.saturating_sub(1);
        id
    }

    fn submit_recovery(
        &mut self,
        keys: Vec<crosspane_types::hid::HidUsage>,
        buttons: Vec<crosspane_types::hid::MouseButton>,
        out: &mut Vec<Output>,
    ) {
        let items = self
            .leases
            .get(&ProjectionId(0))
            .map(|lease| lease.unconfirmed.iter().map(|(&k, &v)| (k, v)).collect())
            .unwrap_or_default();
        let id = self.allocate();
        self.pending.insert(
            id,
            Pending {
                owner: ProjectionId(0),
                items,
            },
        );
        out.push(Output::Inject {
            id,
            cmd: InjectCmd::Recover { keys, buttons },
        });
    }

    fn actions(&mut self, owner: ProjectionId, actions: Vec<Action>, out: &mut Vec<Output>) {
        for action in actions {
            let id = self.allocate();
            let Some(lease) = self.leases.get_mut(&owner) else {
                continue;
            };
            let (item, down) = match action {
                Action::Press(item) => {
                    lease.generations.insert(item, id.0);
                    lease.unconfirmed.remove(&item);
                    if lease.unconfirmed.is_empty() {
                        lease.retry = None;
                    }
                    (item, true)
                }
                Action::Release(item) => {
                    let generation = lease.generations.get(&item).copied().unwrap_or(0);
                    lease.unconfirmed.insert(item, generation);
                    self.pending.insert(
                        id,
                        Pending {
                            owner,
                            items: vec![(item, generation)],
                        },
                    );
                    (item, false)
                }
            };
            let cmd = match item {
                Held::Key(usage) => InjectCmd::Key { usage, down },
                Held::Button(button) => InjectCmd::Button { button, down },
            };
            out.push(Output::Inject { id, cmd });
        }
    }

    pub fn input(
        &mut self,
        owner: ProjectionId,
        item: Held,
        down: bool,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> bool {
        let Some(lease) = self.leases.get_mut(&owner) else {
            return false;
        };
        let deadline = lease_deadline(now);
        if down && lease.ledger.held().is_empty() {
            lease.expires = Some(deadline);
        } else {
            lease.expires.get_or_insert(deadline);
        }
        match lease.ledger.on_input(item, down, now) {
            Ok(action) => {
                self.actions(owner, action.into_iter().collect(), out);
                true
            }
            Err(_) => {
                // Re-open only this scope: release both earlier presses and the possibly torn
                // attempted record. No press is submitted after a journal error.
                let scope = Scoped {
                    shared: self.shared.clone(),
                    owner,
                };
                if let Ok((ledger, items)) = TargetLedger::open(scope) {
                    lease.ledger = ledger;
                    lease.expires = None;
                    self.actions(owner, items.into_iter().map(Action::Release).collect(), out);
                }
                self.cancel_targeting(owner);
                false
            }
        }
    }

    pub fn heartbeat(
        &mut self,
        owner: ProjectionId,
        items: &[Held],
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        // The home entry path drains with this method; ordinary Held reports keep targeting.
        self.cancel_targeting(owner);
        self.input_heartbeat(owner, items, now, out);
    }

    pub fn input_heartbeat(
        &mut self,
        owner: ProjectionId,
        items: &[Held],
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if let Some(lease) = self.leases.get_mut(&owner) {
            lease.expires = Some(lease_deadline(now));
            let actions = lease.ledger.on_heartbeat(items, now);
            self.actions(owner, actions, out);
        }
    }

    pub fn retire(&mut self, owner: ProjectionId, now: MonoTime, out: &mut Vec<Output>) {
        self.cancel_targeting(owner);
        if let Some(lease) = self.leases.get_mut(&owner) {
            lease.retired = true;
            lease.expires = None;
            let actions = lease.ledger.release_all();
            // Absorb a due retry now, before the same dispatch's tick can repeat fresh releases.
            let actions = lease.retry_actions(now, actions);
            self.actions(owner, actions, out);
        }
        self.collect();
    }

    pub fn tick(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        self.expire_leases(now, out);
        let owners: Vec<_> = self.leases.keys().copied().collect();
        for owner in owners {
            if owner == ProjectionId(0) {
                if let Some(lease) = self.leases.get_mut(&owner)
                    && lease.retry.is_some_and(|deadline| deadline <= now)
                {
                    let items: Vec<_> = lease.unconfirmed.keys().copied().collect();
                    let (keys, buttons) = split(&items);
                    lease.retry = now.checked_add(RETRY);
                    self.submit_recovery(keys, buttons, out);
                }
                continue;
            }
            if let Some(lease) = self.leases.get_mut(&owner) {
                let actions = lease.retry_actions(now, Vec::new());
                self.actions(owner, actions, out);
            }
        }
        self.collect();
    }

    /// Called before the source FIFO is resumed, so expired leases release before survivor input.
    pub fn expire_leases(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let owners: Vec<_> = self
            .leases
            .iter()
            .filter(|(_, lease)| lease.expires.is_some_and(|deadline| deadline <= now))
            .map(|(&owner, _)| owner)
            .collect();
        for owner in owners {
            self.cancel_targeting(owner);
            if let Some(lease) = self.leases.get_mut(&owner) {
                lease.expires = None;
                let actions = lease.ledger.release_all();
                let actions = lease.retry_actions(now, actions);
                self.actions(owner, actions, out);
            }
        }
    }

    /// Returns a projection to end if its journal cannot confirm a successful release.
    pub fn done(&mut self, id: InjectId, ok: bool, now: MonoTime) -> Option<ProjectionId> {
        let pending = self.pending.remove(&id)?;
        let lease = self.leases.get_mut(&pending.owner)?;
        let items: Vec<_> = pending
            .items
            .into_iter()
            .filter(|(item, generation)| lease.unconfirmed.get(item) == Some(generation))
            .map(|(item, _)| item)
            .collect();
        if items.is_empty() {
            return None;
        }
        let journal_failed = ok && lease.ledger.confirm_released(&items).is_err();
        if ok && !journal_failed {
            for item in items {
                lease.unconfirmed.remove(&item);
                lease.generations.remove(&item);
            }
            if lease.unconfirmed.is_empty() {
                lease.retry = None;
            }
        } else {
            lease.retry.get_or_insert(now.saturating_add(RETRY));
        }
        self.collect();
        journal_failed.then_some(pending.owner)
    }

    fn collect(&mut self) {
        self.leases
            .retain(|_, lease| !lease.retired || !lease.unconfirmed.is_empty());
        self.pending.retain(|_, pending| {
            self.leases.get(&pending.owner).is_some_and(|lease| {
                pending
                    .items
                    .iter()
                    .any(|(item, generation)| lease.unconfirmed.get(item) == Some(generation))
            })
        });
    }

    pub fn next_deadline(&self) -> Option<MonoTime> {
        self.leases
            .iter()
            .flat_map(|(&owner, lease)| {
                let unissued = self
                    .targeting
                    .as_ref()
                    .is_some_and(|targeting| targeting.owner == owner)
                    || self
                        .queued
                        .iter()
                        .any(|(projection, _)| *projection == owner);
                [
                    lease.ledger.next_deadline(),
                    lease.retry,
                    lease.expires.filter(|_| unissued),
                ]
            })
            .flatten()
            .chain(self.targeting.as_ref().map(|targeting| targeting.deadline))
            .min()
    }
}

fn lease_deadline(now: MonoTime) -> MonoTime {
    now.saturating_add(LEASE_TIMEOUT)
        .saturating_add(Duration::from_nanos(1))
}

pub(super) fn split(
    items: &[Held],
) -> (
    Vec<crosspane_types::hid::HidUsage>,
    Vec<crosspane_types::hid::MouseButton>,
) {
    let mut keys = Vec::new();
    let mut buttons = Vec::new();
    for item in items {
        match item {
            Held::Key(key) => keys.push(*key),
            Held::Button(button) => buttons.push(*button),
        }
    }
    (keys, buttons)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_input::journal::MemoryJournal;
    use crosspane_types::hid::HidUsage;

    #[test]
    fn confirmed_releases_drop_generations_without_clearing_a_later_press() {
        let mut out = Vec::new();
        let mut ledgers = Ledgers::new(Box::new(MemoryJournal::default()), &mut out).unwrap();
        let owner = ProjectionId(1);
        let now = MonoTime::ZERO;
        ledgers.open(owner).unwrap();
        for usage in 4..104 {
            let item = Held::Key(HidUsage::keyboard(usage));
            assert!(ledgers.input(owner, item, true, now, &mut out));
            assert!(ledgers.input(owner, item, false, now, &mut out));
            let Some(Output::Inject { id, .. }) = out.last() else {
                panic!("missing release")
            };
            let id = *id;
            assert_eq!(ledgers.done(id, true, now), None);
            assert!(ledgers.leases[&owner].generations.is_empty());
            out.clear();
        }
        let item = Held::Key(HidUsage::keyboard(4));
        assert!(ledgers.input(owner, item, true, now, &mut out));
        assert!(ledgers.input(owner, item, false, now, &mut out));
        let Some(Output::Inject { id, .. }) = out.last() else {
            panic!("missing release")
        };
        let old = *id;
        assert!(ledgers.input(owner, item, true, now, &mut out));
        ledgers.done(old, true, now);
        assert_eq!(ledgers.leases[&owner].generations.len(), 1);
        assert_eq!(ledgers.leases[&owner].ledger.held(), vec![item]);
    }
}
