//! The single observation ledger in front of core `Observe`.
//!
//! Settings transitions can replay their last admitted observation batch. When the
//! controller has polled in between, that replay is older than what core already holds, and core
//! would reject it and retire every bound proof. The one substitution rule: a non-empty batch with
//! any sample older than the newest admitted sample of its scope is replaced by the newest
//! admitted batch, which is never weaker. Empty batches pass through and withdraw presence.

use std::collections::BTreeMap;

use crosspane_installer_core::CounterSample;
use crosspane_types::id::NodeId;

type Scope = (NodeId, Option<NodeId>);

#[derive(Debug, Default)]
pub struct Ledger {
    latest: Vec<CounterSample>,
    high_water: BTreeMap<Scope, u64>,
}

impl Ledger {
    pub fn prepare(&self, samples: Vec<CounterSample>) -> Vec<CounterSample> {
        let stale = samples.iter().any(|s| {
            let scope = (s.binding.local_node, s.binding.peer);
            self.high_water
                .get(&scope)
                .is_some_and(|newest| s.observed_at_ms < *newest)
        });
        if stale { self.latest.clone() } else { samples }
    }

    /// Record a batch core accepted. The summary always receives exactly this batch.
    pub fn accepted(&mut self, samples: Vec<CounterSample>) {
        for s in &samples {
            let scope = (s.binding.local_node, s.binding.peer);
            let newest = self.high_water.entry(scope).or_insert(0);
            *newest = (*newest).max(s.observed_at_ms);
        }
        self.latest = samples;
    }

    pub fn latest(&self) -> &[CounterSample] {
        &self.latest
    }

    pub fn local(&self) -> Option<&CounterSample> {
        self.latest.iter().find(|s| s.binding.peer.is_none())
    }
}
