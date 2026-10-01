//! The controller's key and button routing table.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

use crosspane_types::id::NodeId;

use crate::Held;

/// The controller's key and button routing table (04 §8 invariant 1).
#[derive(Clone, Debug, Default)]
pub struct Router {
    held: BTreeMap<Held, NodeId>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    /// A captured transition while input is routed to `current`. Returns the node to deliver it
    /// to, or `None` to drop it.
    /// - A down goes to `current` and is recorded. A down for something already held anywhere is
    ///   dropped (no double down).
    /// - An up goes to the node recorded for that item and clears the record. An up for something
    ///   not held is dropped.
    pub fn route(&mut self, item: Held, down: bool, current: NodeId) -> Option<NodeId> {
        if down {
            match self.held.entry(item) {
                Entry::Vacant(entry) => {
                    entry.insert(current);
                    Some(current)
                }
                Entry::Occupied(_) => None,
            }
        } else {
            self.held.remove(&item)
        }
    }

    /// True when no pointer button is held on any node; the engine allows crossing only then.
    pub fn no_buttons_held(&self) -> bool {
        !self.held.keys().any(|item| matches!(item, Held::Button(_)))
    }

    /// Everything currently held on `node`, sorted. Used for heartbeats.
    pub fn held_on(&self, node: NodeId) -> Vec<Held> {
        self.held
            .iter()
            .filter_map(|(&item, &target)| (target == node).then_some(item))
            .collect()
    }

    /// Forget everything held on `node` and return it sorted. The engine sends these as releases
    /// to `node` (session end, link loss, panic). Later ups for these items are dropped by `route`.
    pub fn release_all(&mut self, node: NodeId) -> Vec<Held> {
        let mut released = Vec::new();
        self.held.retain(|&item, &mut target| {
            if target == node {
                released.push(item);
                false
            } else {
                true
            }
        });
        released
    }

    /// Every node with something held, sorted.
    pub fn nodes_with_held(&self) -> Vec<NodeId> {
        self.held
            .values()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}
