//! Node-local physical ownership; journals and logical leases remain with their roles.
//! Release requests, startup journal recovery and home settlement span both logical roles.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crosspane_input::Held;
use crosspane_input::journal::JournalError;
use crosspane_types::id::{NodeId, ProjectionId, SessionId};
use crosspane_types::time::MonoTime;

use crate::io::{InjectCmd, InjectId, Input, Output};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Owner {
    E1(NodeId, SessionId),
    E2(ProjectionId),
}

#[derive(Debug)]
struct Press {
    // The establishing request identifies this physical generation, independently of owners.
    generation: InjectId,
    pending_until: Option<MonoTime>,
}

#[derive(Debug)]
struct Release {
    until: MonoTime,
    ids: VecDeque<InjectId>,
}

#[derive(Clone, Copy)]
pub(crate) enum ReleaseAction {
    Absorb,
    Submit {
        id: InjectId,
        until: MonoTime,
        emit: bool,
    },
}

pub(crate) struct RecoveryRequest {
    pub id: InjectId,
    pub items: Vec<(Held, MonoTime)>,
    emit: bool,
}

impl RecoveryRequest {
    pub(crate) fn output(&self) -> Option<Output> {
        if !self.emit {
            return None;
        }
        let mut keys = Vec::new();
        let mut buttons = Vec::new();
        for &(item, _) in &self.items {
            match item {
                Held::Key(key) => keys.push(key),
                Held::Button(button) => buttons.push(button),
            }
        }
        Some(Output::Inject {
            id: self.id,
            cmd: InjectCmd::Recover { keys, buttons },
        })
    }
}

#[derive(Debug, Default)]
struct State {
    startup: bool,
    owners: BTreeMap<Owner, BTreeSet<Held>>,
    presses: BTreeMap<Held, Press>,
    blocked: BTreeSet<Held>,
    releases: BTreeMap<Held, Release>,
    owing: [BTreeSet<Held>; 2],
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PhysicalInput(Arc<Mutex<State>>);

impl PhysicalInput {
    fn lock(&self) -> Result<MutexGuard<'_, State>, JournalError> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("physical input mutex poisoned").into())
    }

    /// Called only after the role has journaled its logical down.
    pub(crate) fn press(
        &self,
        owner: Owner,
        item: Held,
        id: InjectId,
        pending_until: Option<MonoTime>,
    ) -> Result<bool, JournalError> {
        let mut state = self.lock()?;
        if state.startup || state.blocked.contains(&item) {
            return Ok(false);
        }
        state.owners.entry(owner).or_default().insert(item);
        if state.presses.contains_key(&item) {
            return Ok(false);
        }
        state.releases.remove(&item);
        state.presses.insert(
            item,
            Press {
                generation: id,
                pending_until,
            },
        );
        Ok(true)
    }

    /// A shared request carries every contributing role's independent journal confirmation.
    pub(crate) fn release(
        &self,
        owner: Option<Owner>,
        item: Held,
        id: InjectId,
        now: MonoTime,
    ) -> Result<ReleaseAction, JournalError> {
        let mut state = self.lock()?;
        if let Some(owner) = owner
            && let Some(items) = state.owners.get_mut(&owner)
        {
            items.remove(&item);
        }
        state.owners.retain(|_, items| !items.is_empty());
        if state.owners.values().any(|items| items.contains(&item)) {
            return Ok(ReleaseAction::Absorb);
        }
        state.presses.remove(&item);
        let release = state.releases.entry(item).or_insert(Release {
            until: now,
            ids: VecDeque::new(),
        });
        let emit = release.until <= now;
        if emit {
            release.until = now.saturating_add(Duration::from_millis(50));
            release.ids.push_back(id);
            if release.ids.len() > 8 {
                release.ids.pop_front();
            }
        }
        Ok(ReleaseAction::Submit {
            id: release.ids.back().copied().unwrap_or(id),
            until: release.until,
            emit,
        })
    }

    pub(crate) fn release_member(&self, item: Held, id: InjectId, ok: bool) -> bool {
        self.lock().ok().is_some_and(|state| {
            state.releases.get(&item).is_some_and(|release| {
                release.ids.contains(&id) && (ok || release.ids.back() == Some(&id))
            })
        })
    }

    pub(crate) fn release_failed(&self, item: Held, id: InjectId, now: MonoTime) {
        if let Ok(mut state) = self.lock()
            && let Some(release) = state.releases.get_mut(&item)
            && release.ids.back() == Some(&id)
        {
            release.until = now.saturating_add(Duration::from_millis(50));
        }
    }

    pub(crate) fn detach(&self, owner: Option<Owner>, item: Held) {
        if let Ok(mut state) = self.lock()
            && let Some(items) = owner.and_then(|owner| state.owners.get_mut(&owner))
        {
            items.remove(&item);
            state.owners.retain(|_, items| !items.is_empty());
        }
    }

    pub(crate) fn startup(&self, e1: bool, items: &[Held]) -> Result<(), JournalError> {
        let mut state = self.lock()?;
        state.startup |= !items.is_empty();
        state.owing[usize::from(e1)].extend(items);
        state.blocked.extend(items);
        Ok(())
    }

    pub(crate) fn recovery(
        &self,
        items: &[Held],
        requested: InjectId,
        now: MonoTime,
    ) -> Result<Vec<RecoveryRequest>, JournalError> {
        let mut requests = BTreeMap::new();
        for &item in items {
            if let ReleaseAction::Submit { id, until, emit } =
                self.release(None, item, requested, now)?
            {
                requests
                    .entry(id)
                    .or_insert_with(|| RecoveryRequest {
                        id,
                        items: Vec::new(),
                        emit,
                    })
                    .items
                    .push((item, until));
            }
        }
        Ok(requests.into_values().collect())
    }

    pub(crate) fn blocked(&self, item: Held) -> Result<bool, JournalError> {
        let state = self.lock()?;
        Ok(state.startup || state.blocked.contains(&item))
    }

    pub(crate) fn can_drag(&self, owner: Option<Owner>) -> bool {
        self.lock().ok().is_some_and(|state| {
            !state.startup
                && !state
                    .blocked
                    .contains(&Held::Button(crosspane_types::hid::MouseButton::PRIMARY))
                && !state.owners.iter().any(|(known, items)| {
                    Some(*known) != owner
                        && items.contains(&Held::Button(crosspane_types::hid::MouseButton::PRIMARY))
                })
        })
    }

    pub(crate) fn settled(&self) -> bool {
        self.lock().ok().is_some_and(|state| {
            !state.startup
                && state.owners.is_empty()
                && state.presses.is_empty()
                && state.blocked.is_empty()
                && state.releases.is_empty()
                && state.owing.iter().all(BTreeSet::is_empty)
        })
    }

    pub(crate) fn cleanup(&self, item: Held) -> Result<(), JournalError> {
        let mut state = self.lock()?;
        state.presses.remove(&item);
        state.blocked.insert(item);
        for items in state.owners.values_mut() {
            items.remove(&item);
        }
        state.owners.retain(|_, items| !items.is_empty());
        Ok(())
    }

    /// Exact establishing ID and its stored deadline fence press completions.
    pub(crate) fn press_done(
        &self,
        id: InjectId,
        ok: bool,
        now: MonoTime,
    ) -> Result<Option<(Held, bool)>, JournalError> {
        let mut state = self.lock()?;
        let pending = state.presses.iter().find_map(|(&item, press)| {
            press
                .pending_until
                .filter(|_| press.generation == id)
                .map(|until| (item, until))
        });
        if let Some((item, until)) = pending {
            let confirmed = ok && now < until;
            if confirmed && let Some(press) = state.presses.get_mut(&item) {
                press.pending_until = None;
            }
            return Ok(Some((item, confirmed)));
        }
        Ok(None)
    }

    pub(crate) fn expired(&self, now: MonoTime) -> Result<Vec<Held>, JournalError> {
        Ok(self
            .lock()?
            .presses
            .iter()
            .filter(|(_, press)| press.pending_until.is_some_and(|until| until <= now))
            .map(|(&item, _)| item)
            .collect())
    }

    /// Called before ordinary input, including a late acknowledgement arriving before Tick.
    pub(crate) fn uncertain(&self, input: &Input, now: MonoTime) -> Vec<Held> {
        let mut items = self.expired(now).unwrap_or_default();
        if let Input::InjectDone { id, ok } = input
            && let Ok(Some((item, false))) = self.press_done(*id, *ok, now)
            && !items.contains(&item)
        {
            items.push(item);
        }
        items
    }

    pub(crate) fn retain_blocks(
        &self,
        e1: bool,
        owing: BTreeSet<Held>,
    ) -> Result<(), JournalError> {
        let mut state = self.lock()?;
        state.owing[usize::from(e1)] = owing;
        state.startup &= state.owing.iter().any(|items| !items.is_empty());
        let State {
            owners,
            blocked,
            releases,
            owing,
            ..
        } = &mut *state;
        let owes = |item: &Held| owing.iter().any(|items| items.contains(item));
        releases.retain(|item, _| owes(item));
        blocked.retain(|item| owes(item) || owners.values().any(|items| items.contains(item)));
        Ok(())
    }

    pub(crate) fn next_deadline(&self) -> Result<Option<MonoTime>, JournalError> {
        Ok(self
            .lock()?
            .presses
            .values()
            .filter_map(|press| press.pending_until)
            .min())
    }
}
