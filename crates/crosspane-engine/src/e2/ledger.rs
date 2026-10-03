//! Per-projection leases over a shared journal of the union of held items.
//!
//! Each scoped journal keeps its own ownership set. An up removes the underlying record only
//! after the last owner confirms release. Owner zero is startup recovery. Ended projections keep
//! only their release bookkeeping until confirmation; they cannot receive any further input.
//! Physical downs and ups follow the union of active logical holds. Uncertain presses end all holders
//! and block new downs until cleanup confirms; pending releases retain their journal ownership.

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
const RELEASE_HISTORY: usize = 8;
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
    retry: BTreeMap<Held, (MonoTime, InjectId)>,
    retired: bool,
    // Unlike TargetLedger::next_deadline, this also survives an empty held set.
    expires: Option<MonoTime>,
}

impl Lease {
    fn retry_actions(&mut self, now: MonoTime, mut actions: Vec<Action>) -> Vec<Action> {
        if self.retry.values().any(|(deadline, _)| *deadline <= now) {
            let already: BTreeSet<_> = actions
                .iter()
                .filter_map(|action| match action {
                    Action::Release(item) => Some(*item),
                    _ => None,
                })
                .collect();
            actions.extend(
                self.retry
                    .iter()
                    .filter(|(item, (deadline, _))| *deadline <= now && !already.contains(item))
                    .map(|(&item, _)| Action::Release(item)),
            );
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
    // Pending physical presses time out conservatively; None means confirmed down.
    presses: BTreeMap<Held, Option<(InjectId, MonoTime)>>,
    blocked: BTreeSet<Held>,
    // Kept with the lease so home drains, retirement and lease expiry also cancel unissued input.
    targeting: Option<Targeting>,
    queued: VecDeque<(ProjectionId, ProjInput)>,
    // E1 allocates upward from 1. E2 allocates downward from the other end, so the agent can
    // broadcast InjectDone to both roles without confusing their outstanding requests.
    next_id: u64,
}

impl Ledgers {
    pub fn new(
        journal: Box<dyn Journal>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> Result<Self, JournalError> {
        let recovery = journal.held()?;
        let shared = Arc::new(Mutex::new(Shared {
            journal,
            owners: [(ProjectionId(0), recovery.iter().copied().collect())].into(),
        }));
        let mut this = Self {
            shared,
            leases: BTreeMap::new(),
            pending: BTreeMap::new(),
            presses: BTreeMap::new(),
            blocked: BTreeSet::new(),
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
            this.submit_recovery(keys, buttons, now, out);
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
                    && lease.retry.is_empty()
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
                retry: BTreeMap::new(),
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
        let item = match input {
            ProjInput::Key {
                usage, down: true, ..
            } => Some(Held::Key(*usage)),
            ProjInput::Button {
                button, down: true, ..
            } => Some(Held::Button(*button)),
            _ => None,
        };
        if item.is_some_and(|item| self.blocked.contains(&item)) {
            // Admission is absorbed now; no queue or targeting callback may resurrect this down.
            return Ok(true);
        }
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
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let items: Vec<_> = keys
            .iter()
            .map(|&key| (Held::Key(key), 0))
            .chain(buttons.iter().map(|&button| (Held::Button(button), 0)))
            .collect();
        let id = self.allocate();
        if let Some(lease) = self.leases.get_mut(&ProjectionId(0)) {
            for &(item, _) in &items {
                lease.retry.insert(item, (now.saturating_add(RETRY), id));
            }
        }
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

    fn actions(
        &mut self,
        owner: ProjectionId,
        actions: Vec<Action>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> bool {
        let mut confirmed = true;
        for action in actions {
            let id = self.allocate();
            let item = match action {
                Action::Press(item) | Action::Release(item) => item,
            };
            // The scoped journal also includes unconfirmed ups; only active logical holds keep
            // the physical item down. This check applies to ordinary input and every cleanup/retry.
            let another_holder = self
                .leases
                .iter()
                .any(|(&other, lease)| other != owner && lease.ledger.held().contains(&item));
            let Some(lease) = self.leases.get_mut(&owner) else {
                continue;
            };
            let (item, down) = match action {
                Action::Press(item) => {
                    lease.generations.insert(item, id.0);
                    lease.unconfirmed.remove(&item);
                    lease.retry.remove(&item);
                    (item, true)
                }
                Action::Release(item) => {
                    let generation = lease.generations.get(&item).copied().unwrap_or(0);
                    lease.unconfirmed.insert(item, generation);
                    lease.retry.insert(item, (now.saturating_add(RETRY), id));
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
            if down {
                // Later logical owners coalesce with the pending or confirmed physical press.
                if self.presses.contains_key(&item) {
                    continue;
                }
                self.presses
                    .insert(item, Some((id, now.saturating_add(TARGET_ACK_TIMEOUT))));
            } else {
                if another_holder {
                    // The other holder keeps the journal record. Confirm this owner's absorbed up
                    // through the same checked path as an acknowledged physical release.
                    confirmed &= !self.confirm_release(owner, &[item], true, now);
                    continue;
                }
                self.presses.remove(&item);
            }
            let cmd = match item {
                Held::Key(usage) => InjectCmd::Key { usage, down },
                Held::Button(button) => InjectCmd::Button { button, down },
            };
            out.push(Output::Inject { id, cmd });
        }
        // A retired multi-item scope must survive until all its actions have been processed.
        self.collect();
        confirmed
    }

    pub fn input(
        &mut self,
        owner: ProjectionId,
        item: Held,
        down: bool,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> bool {
        if down && self.blocked.contains(&item) {
            return true; // A later input must pass ordinary targeting after cleanup confirms.
        }
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
            Ok(action) => self.actions(owner, action.into_iter().collect(), now, out),
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
                    self.actions(
                        owner,
                        items.into_iter().map(Action::Release).collect(),
                        now,
                        out,
                    );
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
    ) -> bool {
        if let Some(lease) = self.leases.get_mut(&owner) {
            lease.expires = Some(lease_deadline(now));
            let actions = lease.ledger.on_heartbeat(items, now);
            self.actions(owner, actions, now, out)
        } else {
            false
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
            self.actions(owner, actions, now, out);
        }
        self.collect();
    }

    pub fn tick(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        self.expire_leases(now, out);
        let expired: Vec<_> = self
            .presses
            .iter()
            .filter_map(|(&item, pending)| {
                pending
                    .filter(|(_, deadline)| *deadline <= now)
                    .map(|_| item)
            })
            .collect();
        for item in expired {
            self.cleanup_press(item, now, out);
        }
        let owners: Vec<_> = self.leases.keys().copied().collect();
        for owner in owners {
            if owner == ProjectionId(0) {
                if let Some(lease) = self.leases.get_mut(&owner) {
                    let items: Vec<_> = lease
                        .retry
                        .iter()
                        .filter(|(_, (deadline, _))| *deadline <= now)
                        .map(|(&item, _)| item)
                        .collect();
                    if items.is_empty() {
                        continue;
                    }
                    let (keys, buttons) = split(&items);
                    self.submit_recovery(keys, buttons, now, out);
                }
                continue;
            }
            if let Some(lease) = self.leases.get_mut(&owner) {
                let actions = lease.retry_actions(now, Vec::new());
                self.actions(owner, actions, now, out);
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
                self.actions(owner, actions, now, out);
            }
        }
    }

    fn cleanup_press(&mut self, item: Held, now: MonoTime, out: &mut Vec<Output>) {
        self.presses.remove(&item);
        self.blocked.insert(item);
        let matches_item = |input: &ProjInput| match (input, item) {
            (ProjInput::Key { usage, .. }, Held::Key(key)) => *usage == key,
            (ProjInput::Button { button, .. }, Held::Button(key)) => *button == key,
            _ => false,
        };
        self.queued.retain(|(_, input)| !matches_item(input));
        if self
            .targeting
            .as_ref()
            .is_some_and(|t| matches_item(&t.input))
        {
            self.targeting = None;
        }
        let owners: Vec<_> = self.leases.keys().copied().collect();
        for owner in owners {
            if let Some(lease) = self.leases.get_mut(&owner)
                && let Ok(Some(action)) = lease.ledger.on_input(item, false, now)
            {
                self.actions(owner, vec![action], now, out);
            }
        }
    }

    /// Returns a projection to end if its journal cannot confirm a successful release.
    pub fn done(
        &mut self,
        id: InjectId,
        ok: bool,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> Option<ProjectionId> {
        let press = self.presses.iter().find_map(|(&item, &pending)| {
            pending
                .filter(|(current, _)| *current == id)
                .map(|(_, deadline)| (item, deadline))
        });
        if let Some((item, deadline)) = press {
            if ok && now < deadline {
                self.presses.insert(item, None);
            } else {
                self.cleanup_press(item, now, out);
            }
            return None;
        }
        let pending = self.pending.remove(&id)?;
        let lease = self.leases.get_mut(&pending.owner)?;
        let items: Vec<_> = pending
            .items
            .into_iter()
            .filter(|(item, generation)| {
                lease.unconfirmed.get(item) == Some(generation)
                    && (ok
                        || lease
                            .retry
                            .get(item)
                            .is_some_and(|(_, current)| *current == id))
            })
            .map(|(item, _)| item)
            .collect();
        if items.is_empty() {
            return None;
        }
        let journal_failed = self.confirm_release(pending.owner, &items, ok, now);
        self.collect();
        journal_failed.then_some(pending.owner)
    }

    // Do not collect here: an absorbed up can be one of several actions for a retired scope.
    fn confirm_release(
        &mut self,
        owner: ProjectionId,
        items: &[Held],
        ok: bool,
        now: MonoTime,
    ) -> bool {
        let Some(lease) = self.leases.get_mut(&owner) else {
            return false;
        };
        let journal_failed = ok && lease.ledger.confirm_released(items).is_err();
        if ok && !journal_failed {
            for item in items {
                lease.unconfirmed.remove(item);
                lease.generations.remove(item);
                lease.retry.remove(item);
            }
        } else {
            // A current failed request backs off only its own items, never another item's retry.
            for item in items {
                if let Some((deadline, _)) = lease.retry.get_mut(item) {
                    *deadline = now.saturating_add(RETRY);
                }
            }
        }
        journal_failed
    }

    fn collect(&mut self) {
        self.leases
            .retain(|_, lease| !lease.retired || !lease.unconfirmed.is_empty());
        self.blocked.retain(|item| {
            self.leases.values().any(|lease| {
                lease.unconfirmed.contains_key(item) || lease.ledger.held().contains(item)
            })
        });
        // IDs descend; ordered traversal retains eight newest exact requests per generation.
        let mut histories = BTreeMap::new();
        self.pending.retain(|_, pending| {
            self.leases.get(&pending.owner).is_some_and(|lease| {
                pending.items.retain(|(item, generation)| {
                    let count = histories
                        .entry((pending.owner, *item, *generation))
                        .or_insert(0);
                    *count += 1;
                    lease.unconfirmed.get(item) == Some(generation) && *count <= RELEASE_HISTORY
                });
                !pending.items.is_empty()
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
                    lease.retry.values().map(|(deadline, _)| *deadline).min(),
                    lease.expires.filter(|_| unissued),
                ]
            })
            .flatten()
            .chain(self.targeting.as_ref().map(|targeting| targeting.deadline))
            .chain(
                self.presses
                    .values()
                    .filter_map(|pending| pending.map(|(_, deadline)| deadline)),
            )
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

    const FIRST: ProjectionId = ProjectionId(1);
    const SECOND: ProjectionId = ProjectionId(2);
    const KEY: Held = Held::Key(HidUsage::keyboard(4));
    const BUTTON: Held = Held::Button(crosspane_types::hid::MouseButton::PRIMARY);

    #[derive(Default)]
    struct JournalState {
        held: BTreeSet<Held>,
        records: Vec<(Held, bool)>,
        fail_down: bool,
        fail_up: bool,
    }

    #[derive(Clone, Default)]
    struct RecordedJournal(Arc<Mutex<JournalState>>);

    impl Journal for RecordedJournal {
        fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
            let mut state = self.0.lock().unwrap();
            state.records.push((item, true));
            if state.fail_down {
                return Err(io::Error::other("fake failed down").into());
            }
            state.held.insert(item);
            Ok(())
        }

        fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
            let mut state = self.0.lock().unwrap();
            if state.fail_up {
                return Err(io::Error::other("fake failed up").into());
            }
            state.records.push((item, false));
            state.held.remove(&item);
            Ok(())
        }

        fn held(&self) -> Result<Vec<Held>, JournalError> {
            Ok(self.0.lock().unwrap().held.iter().copied().collect())
        }
    }

    fn time(ms: u64) -> MonoTime {
        MonoTime::from_nanos(ms * 1_000_000)
    }

    fn fixture() -> (Ledgers, RecordedJournal, Vec<Output>) {
        let journal = RecordedJournal::default();
        let mut out = Vec::new();
        let mut ledgers = Ledgers::new(Box::new(journal.clone()), time(0), &mut out).unwrap();
        ledgers.open(FIRST).unwrap();
        ledgers.open(SECOND).unwrap();
        (ledgers, journal, out)
    }

    fn trace(out: &[Output], item: Held) -> Vec<bool> {
        out.iter()
            .filter_map(|output| match (item, output) {
                (
                    Held::Key(wanted),
                    Output::Inject {
                        cmd: InjectCmd::Key { usage, down },
                        ..
                    },
                ) if wanted == *usage => Some(*down),
                (
                    Held::Button(wanted),
                    Output::Inject {
                        cmd: InjectCmd::Button { button, down },
                        ..
                    },
                ) if wanted == *button => Some(*down),
                _ => None,
            })
            .collect()
    }

    fn release_id(out: &[Output]) -> InjectId {
        match out.last().unwrap() {
            Output::Inject { id, .. } => *id,
            _ => panic!("missing release"),
        }
    }

    #[test]
    fn shared_down_is_journaled_before_one_physical_press() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            assert!(ledgers.input(FIRST, item, true, time(0), &mut out));
            assert_eq!(trace(&out, item), vec![true]);
            assert_eq!(journal.0.lock().unwrap().records, vec![(item, true)]);
            out.clear();
            assert!(ledgers.input(SECOND, item, true, time(0), &mut out));
            assert!(out.is_empty(), "later down must be absorbed: {out:?}");
            assert_eq!(journal.0.lock().unwrap().records, vec![(item, true); 2]);
            assert!(ledgers.holds(FIRST, item));
            assert!(ledgers.holds(SECOND, item));
        }
    }

    #[test]
    fn absorbed_up_confirms_only_its_owner_without_physical_or_journal_up() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            for owner in [FIRST, SECOND] {
                assert!(ledgers.input(owner, item, true, time(0), &mut out));
            }
            journal.0.lock().unwrap().fail_up = true;
            out.clear();
            assert!(ledgers.input(FIRST, item, false, time(0), &mut out));
            assert!(out.is_empty(), "earlier up must be absorbed: {out:?}");
            assert!(!ledgers.holds(FIRST, item));
            assert!(ledgers.holds(SECOND, item));
            assert!(ledgers.leases[&FIRST].unconfirmed.is_empty());
            assert!(ledgers.leases[&FIRST].generations.is_empty());
            assert!(ledgers.pending.is_empty());
            assert_eq!(journal.held().unwrap(), vec![item]);
            assert_eq!(journal.0.lock().unwrap().records, vec![(item, true); 2]);
            journal.0.lock().unwrap().fail_up = false;
            assert!(ledgers.input(SECOND, item, false, time(0), &mut out));
            assert_eq!(trace(&out, item), vec![false]);
            assert_eq!(
                journal.held().unwrap(),
                vec![item],
                "until native confirmation"
            );
            ledgers.done(release_id(&out), true, time(0), &mut out);
            assert!(journal.held().unwrap().is_empty());
            assert!(ledgers.settled());
        }
    }

    #[test]
    fn simultaneous_retirements_submit_only_the_final_owners_up() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            for owner in [FIRST, SECOND] {
                assert!(ledgers.input(owner, item, true, time(0), &mut out));
            }
            out.clear();
            ledgers.retire(FIRST, time(0), &mut out);
            ledgers.retire(SECOND, time(0), &mut out);
            assert_eq!(trace(&out, item), vec![false]);
            assert!(!ledgers.leases.contains_key(&FIRST));
            assert_eq!(journal.held().unwrap(), vec![item]);
            ledgers.done(release_id(&out), true, time(0), &mut out);
            assert!(journal.held().unwrap().is_empty());
            assert!(ledgers.settled());
        }
    }

    #[test]
    fn retiring_a_shared_multi_item_scope_confirms_every_absorbed_release() {
        let (mut ledgers, journal, mut out) = fixture();
        for owner in [FIRST, SECOND] {
            for item in [KEY, BUTTON] {
                assert!(ledgers.input(owner, item, true, time(0), &mut out));
            }
        }
        out.clear();
        ledgers.retire(FIRST, time(0), &mut out);
        assert!(out.is_empty());
        assert!(!ledgers.leases.contains_key(&FIRST));
        assert!(!ledgers.shared.lock().unwrap().owners.contains_key(&FIRST));
        ledgers.retire(SECOND, time(0), &mut out);
        assert_eq!(trace(&out, KEY), vec![false]);
        assert_eq!(trace(&out, BUTTON), vec![false]);
        let ids: Vec<_> = out
            .iter()
            .filter_map(|output| match output {
                Output::Inject { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        for id in ids {
            ledgers.done(id, true, time(0), &mut out);
        }
        assert!(journal.held().unwrap().is_empty());
        assert!(ledgers.settled());
    }

    #[test]
    fn simultaneous_explicit_ups_and_home_drains_submit_one_physical_up() {
        for item in [KEY, BUTTON] {
            for heartbeat in [false, true] {
                let (mut ledgers, journal, mut out) = fixture();
                for owner in [FIRST, SECOND] {
                    assert!(ledgers.input(owner, item, true, time(0), &mut out));
                }
                out.clear();
                for owner in [FIRST, SECOND] {
                    if heartbeat {
                        ledgers.heartbeat(owner, &[], time(0), &mut out);
                    } else {
                        assert!(ledgers.input(owner, item, false, time(0), &mut out));
                    }
                }
                assert_eq!(trace(&out, item), vec![false]);
                ledgers.done(release_id(&out), true, time(0), &mut out);
                assert!(journal.held().unwrap().is_empty());
                assert!(ledgers.settled());
            }
        }
    }

    #[test]
    fn successful_final_up_with_failed_journal_confirmation_still_retries() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            for owner in [FIRST, SECOND] {
                assert!(ledgers.input(owner, item, true, time(0), &mut out));
            }
            out.clear();
            ledgers.retire(FIRST, time(0), &mut out);
            ledgers.retire(SECOND, time(0), &mut out);
            journal.0.lock().unwrap().fail_up = true;
            assert_eq!(
                ledgers.done(release_id(&out), true, time(0), &mut out),
                Some(SECOND)
            );
            assert_eq!(journal.held().unwrap(), vec![item]);
            out.clear();
            ledgers.tick(time(50), &mut out);
            assert_eq!(trace(&out, item), vec![false]);
            journal.0.lock().unwrap().fail_up = false;
            ledgers.done(release_id(&out), true, time(50), &mut out);
            assert!(ledgers.settled());
            assert!(journal.held().unwrap().is_empty());
        }
    }

    #[test]
    fn final_owner_failed_release_retries_and_retains_the_journal() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            for owner in [FIRST, SECOND] {
                assert!(ledgers.input(owner, item, true, time(0), &mut out));
            }
            out.clear();
            ledgers.retire(FIRST, time(0), &mut out);
            ledgers.retire(SECOND, time(0), &mut out);
            let id = release_id(&out);
            assert_eq!(trace(&out, item), vec![false]);
            ledgers.done(id, false, time(0), &mut out);
            assert_eq!(journal.held().unwrap(), vec![item]);
            out.clear();
            ledgers.tick(time(49), &mut out);
            assert!(out.is_empty());
            ledgers.tick(time(50), &mut out);
            assert_eq!(trace(&out, item), vec![false]);
            assert_ne!(release_id(&out), id);
            ledgers.done(release_id(&out), true, time(50), &mut out);
            assert!(journal.held().unwrap().is_empty());
            assert!(ledgers.settled());
        }
    }

    #[test]
    fn stale_failed_release_retry_is_absorbed_when_a_new_owner_holds() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            assert!(ledgers.input(FIRST, item, true, time(0), &mut out));
            out.clear();
            ledgers.retire(FIRST, time(0), &mut out);
            let old = release_id(&out);
            ledgers.done(old, false, time(0), &mut out);
            out.clear();
            assert!(ledgers.input(SECOND, item, true, time(10), &mut out));
            assert_eq!(trace(&out, item), vec![true]);
            out.clear();
            ledgers.tick(time(50), &mut out);
            assert!(
                out.is_empty(),
                "old retry must not lift the new hold: {out:?}"
            );
            assert!(!ledgers.leases.contains_key(&FIRST));
            ledgers.done(old, true, time(51), &mut out);
            assert!(ledgers.holds(SECOND, item));
            assert_eq!(journal.held().unwrap(), vec![item]);
            ledgers.retire(SECOND, time(52), &mut out);
            assert_eq!(trace(&out, item), vec![false]);
            ledgers.done(release_id(&out), true, time(52), &mut out);
            assert!(ledgers.settled());
        }
    }

    #[test]
    fn late_generation_callback_preserves_repress_and_another_owner() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            assert!(ledgers.input(FIRST, item, true, time(0), &mut out));
            assert!(ledgers.input(FIRST, item, false, time(0), &mut out));
            let old = release_id(&out);
            assert!(ledgers.input(FIRST, item, true, time(1), &mut out));
            assert!(ledgers.input(SECOND, item, true, time(1), &mut out));
            let generation = ledgers.leases[&FIRST].generations[&item];
            ledgers.done(old, true, time(2), &mut out);
            assert_eq!(ledgers.leases[&FIRST].generations[&item], generation);
            assert!(ledgers.holds(FIRST, item));
            assert!(ledgers.holds(SECOND, item));
            out.clear();
            ledgers.retire(FIRST, time(3), &mut out);
            assert!(out.is_empty());
            ledgers.done(old, false, time(4), &mut out);
            ledgers.tick(time(54), &mut out);
            assert!(out.is_empty());
            assert_eq!(journal.held().unwrap(), vec![item]);
            ledgers.retire(SECOND, time(55), &mut out);
            assert_eq!(trace(&out, item), vec![false]);
        }
    }

    #[test]
    fn failed_later_down_cleans_its_scope_without_lifting_the_first_owner() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            assert!(ledgers.input(FIRST, item, true, time(0), &mut out));
            journal.0.lock().unwrap().fail_down = true;
            out.clear();
            assert!(!ledgers.input(SECOND, item, true, time(0), &mut out));
            assert!(
                out.is_empty(),
                "failed later journal write must not release first owner"
            );
            assert!(ledgers.holds(FIRST, item));
            assert!(ledgers.leases[&SECOND].unconfirmed.is_empty());
            assert_eq!(journal.held().unwrap(), vec![item]);
        }
    }

    #[test]
    fn crash_with_two_owners_recovers_one_up_per_journaled_item() {
        let (mut ledgers, journal, mut out) = fixture();
        for owner in [FIRST, SECOND] {
            for item in [KEY, BUTTON] {
                assert!(ledgers.input(owner, item, true, time(0), &mut out));
            }
        }
        assert_eq!(journal.held().unwrap(), vec![KEY, BUTTON]);
        drop(ledgers);
        out.clear();
        let mut replay = Ledgers::new(Box::new(journal.clone()), time(0), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Output::Inject {
            cmd: InjectCmd::Recover { keys, buttons }, ..
        } if keys == &[HidUsage::keyboard(4)] && buttons == &[crosspane_types::hid::MouseButton::PRIMARY]));
        assert!(!replay.recovery_done());
        replay.done(release_id(&out), false, time(0), &mut out);
        out.clear();
        replay.tick(time(50), &mut out);
        assert_eq!(out.len(), 1);
        replay.done(release_id(&out), true, time(50), &mut out);
        assert!(replay.recovery_done());
        assert!(journal.held().unwrap().is_empty());
        out.clear();
        let _ = Ledgers::new(Box::new(journal), time(0), &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn unanswered_final_up_retries_until_the_shared_drain_settles() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            for owner in [FIRST, SECOND] {
                assert!(ledgers.input(owner, item, true, time(0), &mut out));
            }
            out.clear();
            ledgers.retire(FIRST, time(0), &mut out);
            ledgers.retire(SECOND, time(0), &mut out);
            assert_eq!(trace(&out, item), vec![false]);
            let unanswered = release_id(&out);
            assert!(!ledgers.settled());
            assert_eq!(ledgers.next_deadline(), Some(time(50)));
            out.clear();
            ledgers.tick(time(49), &mut out);
            assert!(out.is_empty());
            ledgers.tick(time(50), &mut out);
            assert_eq!(trace(&out, item), vec![false]);
            assert_ne!(release_id(&out), unanswered);
            assert_eq!(journal.held().unwrap(), vec![item]);
            ledgers.done(release_id(&out), true, time(50), &mut out);
            assert!(ledgers.settled(), "home drain can now advance");
            assert!(journal.held().unwrap().is_empty());
            ledgers.done(unanswered, false, time(51), &mut out);
            out.clear();
            ledgers.tick(time(101), &mut out);
            assert!(out.is_empty());
        }
    }

    #[test]
    fn unanswered_startup_recovery_retries_without_admitting_new_input() {
        let journal = RecordedJournal::default();
        journal.0.lock().unwrap().held.extend([KEY, BUTTON]);
        let mut out = Vec::new();
        let mut ledgers = Ledgers::new(Box::new(journal.clone()), time(0), &mut out).unwrap();
        let initial = release_id(&out);
        assert_eq!(ledgers.next_deadline(), Some(time(50)));
        assert!(!ledgers.recovery_done());
        out.clear();
        ledgers.tick(time(50), &mut out);
        assert_eq!(out.len(), 1);
        assert_ne!(release_id(&out), initial);
        assert!(matches!(&out[0], Output::Inject {
            cmd: InjectCmd::Recover { keys, buttons }, ..
        } if keys.len() == 1 && buttons.len() == 1));
        ledgers.done(release_id(&out), true, time(50), &mut out);
        assert!(ledgers.recovery_done());
        assert!(ledgers.settled());
        assert!(journal.held().unwrap().is_empty());
        ledgers.done(initial, false, time(51), &mut out);
        out.clear();
        ledgers.tick(time(101), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn startup_recovery_deadline_is_relative_to_the_existing_engine_clock() {
        let journal = RecordedJournal::default();
        journal.0.lock().unwrap().held.insert(KEY);
        let (e2, out) = super::super::E2::new(
            &crate::EngineConfig::new(crosspane_types::id::NodeId([1; 32])),
            Box::new(journal),
            time(200),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(e2.next_deadline(), Some(time(250)));
    }

    #[test]
    fn failed_press_cleans_possibly_applied_down_before_another_owner() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            assert!(ledgers.input(FIRST, item, true, time(0), &mut out));
            let failed = release_id(&out);
            ledgers.done(failed, false, time(1), &mut out);
            ledgers.tick(time(1), &mut out);
            assert_eq!(trace(&out, item), vec![true, false]);
            let cleanup = release_id(&out);
            assert!(!ledgers.holds(FIRST, item));
            out.clear();
            assert!(ledgers.input(SECOND, item, true, time(2), &mut out));
            assert!(out.is_empty(), "cleanup must confirm before another press");
            assert!(!ledgers.holds(SECOND, item));
            assert_eq!(journal.held().unwrap(), vec![item]);
            ledgers.done(cleanup, true, time(3), &mut out);
            assert!(ledgers.input(SECOND, item, true, time(4), &mut out));
            assert_eq!(trace(&out, item), vec![true]);
            let established = release_id(&out);
            ledgers.done(established, true, time(4), &mut out);
            ledgers.done(failed, false, time(5), &mut out);
            out.clear();
            ledgers.tick(time(51), &mut out);
            assert!(out.is_empty(), "old failure cannot invalidate a new press");
            ledgers.retire(FIRST, time(52), &mut out);
            assert!(out.is_empty());
            ledgers.retire(SECOND, time(52), &mut out);
            assert_eq!(trace(&out, item), vec![false]);
            ledgers.done(release_id(&out), true, time(52), &mut out);
            assert!(journal.held().unwrap().is_empty());
            assert!(ledgers.settled());
        }
    }

    #[test]
    fn failed_shared_press_ends_every_logical_owner_without_repress() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            assert!(ledgers.input(FIRST, item, true, time(0), &mut out));
            let failed = release_id(&out);
            assert!(ledgers.input(SECOND, item, true, time(0), &mut out));
            ledgers.done(failed, false, time(1), &mut out);
            ledgers.tick(time(1), &mut out);
            assert_eq!(trace(&out, item), vec![true, false]);
            let cleanup = release_id(&out);
            assert!(!ledgers.holds(FIRST, item));
            assert!(!ledgers.holds(SECOND, item));
            assert_eq!(journal.held().unwrap(), vec![item]);
            ledgers.done(cleanup, true, time(2), &mut out);
            out.clear();
            for owner in [FIRST, SECOND] {
                assert!(ledgers.input(owner, item, false, time(3), &mut out));
            }
            ledgers.tick(time(51), &mut out);
            assert!(
                out.is_empty(),
                "later ups and ticks cannot re-press the item"
            );
            assert!(journal.held().unwrap().is_empty());
            ledgers.retire(FIRST, time(103), &mut out);
            ledgers.retire(SECOND, time(103), &mut out);
            assert!(out.is_empty());
            assert!(ledgers.settled());
        }
    }

    #[test]
    fn uncertain_press_timeout_ends_every_owner_without_repress() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            for owner in [FIRST, SECOND] {
                assert!(ledgers.input(owner, item, true, time(0), &mut out));
            }
            for owner in [FIRST, SECOND] {
                ledgers.heartbeat(owner, &[item], time(499), &mut out);
            }
            let old_press = release_id(&out);
            out.clear();
            ledgers.tick(time(499), &mut out);
            assert!(out.is_empty());
            ledgers.tick(time(500), &mut out);
            assert_eq!(trace(&out, item), vec![false]);
            assert!(!ledgers.holds(FIRST, item));
            assert!(!ledgers.holds(SECOND, item));
            let cleanup = release_id(&out);
            ledgers.done(old_press, true, time(501), &mut out);
            ledgers.done(cleanup, true, time(501), &mut out);
            out.clear();
            ledgers.tick(time(551), &mut out);
            assert!(out.is_empty());
            assert!(journal.held().unwrap().is_empty());
        }
    }

    #[test]
    fn failed_key_release_does_not_postpone_button_retry() {
        let (mut ledgers, journal, mut out) = fixture();
        for item in [KEY, BUTTON] {
            ledgers.input(FIRST, item, true, time(0), &mut out);
            ledgers.done(release_id(&out), true, time(0), &mut out);
        }
        out.clear();
        ledgers.retire(FIRST, time(0), &mut out);
        let key_up = out
            .iter()
            .find_map(|o| match o {
                Output::Inject {
                    id,
                    cmd: InjectCmd::Key { down: false, .. },
                } => Some(*id),
                _ => None,
            })
            .unwrap();
        ledgers.done(key_up, false, time(49), &mut out);
        out.clear();
        ledgers.tick(time(50), &mut out);
        assert_eq!(trace(&out, BUTTON), vec![false]);
        assert!(trace(&out, KEY).is_empty());
        ledgers.done(release_id(&out), true, time(50), &mut out);
        out.clear();
        ledgers.tick(time(99), &mut out);
        assert_eq!(trace(&out, KEY), vec![false]);
        ledgers.done(release_id(&out), true, time(99), &mut out);
        assert!(journal.held().unwrap().is_empty());
    }

    #[test]
    fn superseded_release_failure_does_not_postpone_retry() {
        let (mut ledgers, _, mut out) = fixture();
        ledgers.input(FIRST, KEY, true, time(0), &mut out);
        ledgers.done(release_id(&out), true, time(0), &mut out);
        out.clear();
        ledgers.retire(FIRST, time(0), &mut out);
        let old = release_id(&out);
        out.clear();
        ledgers.tick(time(50), &mut out);
        assert_eq!(trace(&out, KEY), vec![false]);
        ledgers.done(old, false, time(51), &mut out);
        out.clear();
        ledgers.tick(time(100), &mut out);
        assert_eq!(trace(&out, KEY), vec![false]);
    }

    #[test]
    fn superseded_recovery_failure_does_not_postpone_retry() {
        let journal = RecordedJournal::default();
        journal.0.lock().unwrap().held.extend([KEY, BUTTON]);
        let mut out = Vec::new();
        let mut ledgers = Ledgers::new(Box::new(journal), time(0), &mut out).unwrap();
        let old = release_id(&out);
        out.clear();
        ledgers.tick(time(50), &mut out);
        assert_eq!(out.len(), 1);
        ledgers.done(old, false, time(51), &mut out);
        out.clear();
        ledgers.tick(time(100), &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Output::Inject {
            cmd: InjectCmd::Recover { keys, buttons }, ..
        } if keys.len() == 1 && buttons.len() == 1));
    }

    #[test]
    fn late_press_success_before_tick_takes_cleanup() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            ledgers.input(FIRST, item, true, time(0), &mut out);
            let late = release_id(&out);
            ledgers.input(SECOND, item, true, time(0), &mut out);
            for owner in [FIRST, SECOND] {
                ledgers.heartbeat(owner, &[item], time(499), &mut out);
            }
            out.clear();
            ledgers.done(late, true, time(501), &mut out);
            assert_eq!(
                trace(&out, item),
                vec![false],
                "late success cannot defeat timeout"
            );
            assert!(!ledgers.holds(FIRST, item));
            assert!(!ledgers.holds(SECOND, item));
            assert_eq!(journal.held().unwrap(), vec![item]);
            ledgers.done(release_id(&out), true, time(502), &mut out);
            out.clear();
            ledgers.tick(time(552), &mut out);
            assert!(out.is_empty());
            assert!(journal.held().unwrap().is_empty());
        }
    }

    #[test]
    fn unanswered_release_pending_state_stays_bounded() {
        let (mut ledgers, _, mut out) = fixture();
        ledgers.input(FIRST, KEY, true, time(0), &mut out);
        ledgers.done(release_id(&out), true, time(0), &mut out);
        out.clear();
        ledgers.retire(FIRST, time(0), &mut out);
        let mut recent = VecDeque::from([release_id(&out)]);
        for n in 1..=1_000 {
            out.clear();
            ledgers.tick(time(n * 50), &mut out);
            assert_eq!(trace(&out, KEY), vec![false]);
            recent.push_back(release_id(&out));
            if recent.len() > 8 {
                recent.pop_front();
            }
            assert_eq!(
                ledgers.pending.keys().copied().collect::<Vec<_>>(),
                recent.iter().rev().copied().collect::<Vec<_>>(),
                "eight exact release ids at retry {n}"
            );
            assert!(!ledgers.settled());
        }
        ledgers.done(release_id(&out), true, time(50_000), &mut out);
        assert!(ledgers.settled());
    }

    #[test]
    fn unanswered_recovery_pending_state_stays_bounded() {
        let journal = RecordedJournal::default();
        journal.0.lock().unwrap().held.extend([KEY, BUTTON]);
        let mut out = Vec::new();
        let mut ledgers = Ledgers::new(Box::new(journal), time(0), &mut out).unwrap();
        let mut recent = VecDeque::from([release_id(&out)]);
        for n in 1..=1_000 {
            out.clear();
            ledgers.tick(time(n * 50), &mut out);
            assert_eq!(out.len(), 1);
            recent.push_back(release_id(&out));
            if recent.len() > 8 {
                recent.pop_front();
            }
            assert_eq!(
                ledgers.pending.keys().copied().collect::<Vec<_>>(),
                recent.iter().rev().copied().collect::<Vec<_>>(),
                "eight exact recovery ids at retry {n}"
            );
            assert!(
                ledgers
                    .pending
                    .values()
                    .all(|pending| pending.items.len() == 2)
            );
            assert!(!ledgers.recovery_done());
        }
        ledgers.done(release_id(&out), true, time(50_000), &mut out);
        assert!(ledgers.settled());
    }

    #[test]
    fn retained_release_success_confirms_same_generation_cleanup() {
        let (mut ledgers, journal, mut out) = fixture();
        ledgers.input(FIRST, KEY, true, time(0), &mut out);
        ledgers.done(release_id(&out), true, time(0), &mut out);
        out.clear();
        ledgers.retire(FIRST, time(0), &mut out);
        let old = release_id(&out);
        out.clear();
        ledgers.tick(time(50), &mut out);
        let current = release_id(&out);
        ledgers.done(old, true, time(51), &mut out);
        assert!(journal.held().unwrap().is_empty());
        assert!(ledgers.settled());
        ledgers.done(current, false, time(52), &mut out);
        out.clear();
        ledgers.tick(time(102), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn retained_recovery_success_confirms_same_generation_cleanup() {
        let journal = RecordedJournal::default();
        journal.0.lock().unwrap().held.extend([KEY, BUTTON]);
        let mut out = Vec::new();
        let mut ledgers = Ledgers::new(Box::new(journal.clone()), time(0), &mut out).unwrap();
        let old = release_id(&out);
        out.clear();
        ledgers.tick(time(50), &mut out);
        let current = release_id(&out);
        ledgers.done(old, true, time(51), &mut out);
        assert!(journal.held().unwrap().is_empty());
        assert!(ledgers.settled());
        ledgers.done(current, false, time(52), &mut out);
        out.clear();
        ledgers.tick(time(102), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn unrelated_success_inside_release_id_interval_cannot_confirm_cleanup() {
        let (mut ledgers, journal, mut out) = fixture();
        ledgers.input(FIRST, KEY, true, time(0), &mut out);
        ledgers.done(release_id(&out), true, time(0), &mut out);
        out.clear();
        ledgers.retire(FIRST, time(0), &mut out);
        let first_up = release_id(&out);
        out.clear();
        ledgers.inject(
            InjectCmd::MoveTo {
                display: crosspane_types::id::DisplayId(7),
                position: crosspane_types::geom::PointDevice::new(10.0, 20.0),
            },
            &mut out,
        );
        let unrelated_motion = release_id(&out);
        out.clear();
        ledgers.input(SECOND, BUTTON, true, time(1), &mut out);
        let unrelated_press = release_id(&out);
        ledgers.done(unrelated_press, true, time(1), &mut out);
        out.clear();
        ledgers.tick(time(50), &mut out);
        let current_up = release_id(&out);
        assert!(first_up.0 > unrelated_motion.0);
        assert!(unrelated_motion.0 > unrelated_press.0);
        assert!(unrelated_press.0 > current_up.0);
        // Both unrelated requests lie in the KEY release's global [first,current] id interval.
        out.clear();
        for id in [unrelated_motion, unrelated_press] {
            ledgers.done(id, true, time(51), &mut out);
            assert!(out.is_empty());
            assert_eq!(journal.held().unwrap(), vec![KEY, BUTTON]);
        }
        ledgers.done(current_up, true, time(52), &mut out);
        assert_eq!(journal.held().unwrap(), vec![BUTTON]);
        ledgers.retire(SECOND, time(53), &mut out);
        ledgers.done(release_id(&out), true, time(53), &mut out);
        assert!(ledgers.settled());
        assert!(journal.held().unwrap().is_empty());
    }

    #[test]
    fn evicted_release_success_is_unknown_but_retained_older_success_confirms() {
        for item in [KEY, BUTTON] {
            let (mut ledgers, journal, mut out) = fixture();
            ledgers.input(FIRST, item, true, time(0), &mut out);
            ledgers.done(release_id(&out), true, time(0), &mut out);
            out.clear();
            ledgers.retire(FIRST, time(0), &mut out);
            let evicted = release_id(&out);
            let mut oldest_retained = evicted;
            for n in 1..=8 {
                out.clear();
                ledgers.tick(time(n * 50), &mut out);
                assert_eq!(trace(&out, item), vec![false]);
                if n == 1 {
                    oldest_retained = release_id(&out);
                }
            }
            assert_eq!(ledgers.pending.len(), 8);
            assert!(!ledgers.pending.contains_key(&evicted));
            assert!(ledgers.pending.contains_key(&oldest_retained));
            ledgers.done(evicted, true, time(401), &mut out);
            assert_eq!(journal.held().unwrap(), vec![item]);
            assert!(!ledgers.settled());
            assert_eq!(ledgers.next_deadline(), Some(time(450)));
            ledgers.done(oldest_retained, true, time(402), &mut out);
            assert!(journal.held().unwrap().is_empty());
            assert!(ledgers.settled());
            out.clear();
            ledgers.tick(time(500), &mut out);
            assert!(out.is_empty());
        }
    }

    #[test]
    fn confirmed_releases_drop_generations_without_clearing_a_later_press() {
        let mut out = Vec::new();
        let mut ledgers =
            Ledgers::new(Box::new(MemoryJournal::default()), MonoTime::ZERO, &mut out).unwrap();
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
            assert_eq!(ledgers.done(id, true, now, &mut out), None);
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
        ledgers.done(old, true, now, &mut out);
        assert_eq!(ledgers.leases[&owner].generations.len(), 1);
        assert_eq!(ledgers.leases[&owner].ledger.held(), vec![item]);
    }
}
