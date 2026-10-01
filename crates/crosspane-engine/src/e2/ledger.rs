//! Per-projection leases over a shared journal of the union of held items.
//!
//! Each scoped journal keeps its own ownership set. An up removes the underlying record only
//! after the last owner confirms release. Owner zero is startup recovery. Ended projections keep
//! only their release bookkeeping until confirmation; they cannot receive any further input.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crosspane_input::Held;
use crosspane_input::journal::{Journal, JournalError};
use crosspane_input::lease::{Action, TargetLedger};
use crosspane_types::id::ProjectionId;
use crosspane_types::time::MonoTime;

use crate::io::{InjectCmd, InjectId, Output};

const RETRY: Duration = Duration::from_millis(50);

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
}

struct Pending {
    owner: ProjectionId,
    items: Vec<(Held, u64)>,
}

pub(super) struct Ledgers {
    shared: Arc<Mutex<Shared>>,
    leases: BTreeMap<ProjectionId, Lease>,
    pending: BTreeMap<InjectId, Pending>,
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

    pub fn open(&mut self, owner: ProjectionId) -> Result<(), JournalError> {
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
            },
        );
        Ok(())
    }

    pub fn inject(&mut self, cmd: InjectCmd, out: &mut Vec<Output>) {
        let id = self.allocate();
        out.push(Output::Inject { id, cmd });
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
                    self.actions(owner, items.into_iter().map(Action::Release).collect(), out);
                }
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
        if let Some(lease) = self.leases.get_mut(&owner) {
            let actions = lease.ledger.on_heartbeat(items, now);
            self.actions(owner, actions, out);
        }
    }

    pub fn retire(&mut self, owner: ProjectionId, out: &mut Vec<Output>) {
        if let Some(lease) = self.leases.get_mut(&owner) {
            lease.retired = true;
            let actions = lease.ledger.release_all();
            self.actions(owner, actions, out);
        }
        self.collect();
    }

    pub fn tick(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let owners: Vec<_> = self.leases.keys().copied().collect();
        for owner in owners {
            if let Some(lease) = self.leases.get_mut(&owner) {
                let mut actions = lease.ledger.on_tick(now);
                if lease.retry.is_some_and(|deadline| deadline <= now) {
                    let already: BTreeSet<_> = actions
                        .iter()
                        .filter_map(|a| match a {
                            Action::Release(item) => Some(*item),
                            _ => None,
                        })
                        .collect();
                    actions.extend(
                        lease
                            .unconfirmed
                            .keys()
                            .filter(|item| !already.contains(item))
                            .copied()
                            .map(Action::Release),
                    );
                    lease.retry = now.checked_add(RETRY);
                }
                self.actions(owner, actions, out);
            }
        }
        self.collect();
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
            .values()
            .flat_map(|lease| [lease.ledger.next_deadline(), lease.retry])
            .flatten()
            .min()
    }
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
