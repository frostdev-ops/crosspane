use crate::AttemptId;
use crosspane_types::id::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CounterId(pub u16);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Epochs {
    pub instance_id: u64,
    pub gate_epoch: u64,
    pub grants_epoch: u64,
    pub layout_epoch: u64,
    pub backends_epoch: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochDependencies {
    pub gate: bool,
    pub grants: bool,
    pub layout: bool,
    pub backends: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBinding {
    pub local_node: NodeId,
    pub peer: Option<NodeId>,
    pub link_generation: Option<u64>,
    pub epochs: Epochs,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationSource {
    Live,
    Demo,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterSample {
    pub source: ObservationSource,
    pub observed_at_ms: u64,
    pub binding: EvidenceBinding,
    pub values: BTreeMap<CounterId, u64>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityProof {
    inner: Box<ProofData>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProofData {
    attempt: AttemptId,
    end: CounterSample,
    dependencies: EpochDependencies,
}
impl ActivityProof {
    pub(crate) fn end(&self) -> &CounterSample {
        &self.inner.end
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EvidenceError {
    #[error("evidence is not live")]
    NotLive,
    #[error("evidence binding differs")]
    WrongBinding,
    #[error("a relevant epoch changed")]
    EpochChanged,
    #[error("a required counter is missing")]
    MissingCounter,
    #[error("a counter regressed")]
    CounterRegressed,
    #[error("a required counter did not advance")]
    CounterDidNotAdvance,
    #[error("a forbidden counter advanced")]
    ForbiddenCounterAdvanced,
    #[error("observation time is invalid")]
    InvalidTime,
    #[error("no advancing counter is required")]
    EmptyRequirements,
    #[error("counter requirements conflict or repeat")]
    InvalidRequirements,
}

impl EvidenceBinding {
    pub(crate) fn scope(&self) -> (NodeId, Option<NodeId>) {
        (self.local_node, self.peer)
    }
    pub(crate) fn well_formed(&self) -> bool {
        self.peer.is_some() == self.link_generation.is_some()
    }
    pub(crate) fn matches(&self, other: &Self, dependencies: EpochDependencies) -> bool {
        self.well_formed()
            && other.well_formed()
            && self.scope() == other.scope()
            && self.link_generation == other.link_generation
            && self.epochs.instance_id == other.epochs.instance_id
            && (!dependencies.gate || self.epochs.gate_epoch == other.epochs.gate_epoch)
            && (!dependencies.grants || self.epochs.grants_epoch == other.epochs.grants_epoch)
            && (!dependencies.layout || self.epochs.layout_epoch == other.epochs.layout_epoch)
            && (!dependencies.backends || self.epochs.backends_epoch == other.epochs.backends_epoch)
    }
}

pub fn check_activity(
    attempt: AttemptId,
    start: &CounterSample,
    end: &CounterSample,
    dependencies: EpochDependencies,
    must_advance: &[CounterId],
    must_not_advance: &[CounterId],
) -> Result<ActivityProof, EvidenceError> {
    if start.source != ObservationSource::Live || end.source != ObservationSource::Live {
        return Err(EvidenceError::NotLive);
    }
    if end.observed_at_ms < start.observed_at_ms {
        return Err(EvidenceError::InvalidTime);
    }
    let no_epochs = EpochDependencies {
        gate: false,
        grants: false,
        layout: false,
        backends: false,
    };
    if !start.binding.matches(&end.binding, no_epochs) {
        return Err(EvidenceError::WrongBinding);
    }
    if !start.binding.matches(&end.binding, dependencies) {
        return Err(EvidenceError::EpochChanged);
    }
    if must_advance.is_empty() {
        return Err(EvidenceError::EmptyRequirements);
    }
    let required: BTreeSet<_> = must_advance
        .iter()
        .chain(must_not_advance)
        .copied()
        .collect();
    if required.len() != must_advance.len() + must_not_advance.len() {
        return Err(EvidenceError::InvalidRequirements);
    }
    for (key, old) in &start.values {
        let new = end.values.get(key).ok_or(EvidenceError::MissingCounter)?;
        if new < old {
            return Err(EvidenceError::CounterRegressed);
        }
    }
    for key in required {
        let old = start
            .values
            .get(&key)
            .ok_or(EvidenceError::MissingCounter)?;
        let new = end.values.get(&key).ok_or(EvidenceError::MissingCounter)?;
        if must_advance.contains(&key) && new == old {
            return Err(EvidenceError::CounterDidNotAdvance);
        }
        if must_not_advance.contains(&key) && new != old {
            return Err(EvidenceError::ForbiddenCounterAdvanced);
        }
    }
    Ok(ActivityProof {
        inner: Box::new(ProofData {
            attempt,
            end: end.clone(),
            dependencies,
        }),
    })
}

pub fn activity_is_current(proof: &ActivityProof, current: &CounterSample) -> bool {
    current.source == ObservationSource::Live
        && current.observed_at_ms >= proof.end().observed_at_ms
        && proof
            .end()
            .binding
            .matches(&current.binding, proof.inner.dependencies)
        && proof
            .end()
            .values
            .iter()
            .all(|(key, old)| current.values.get(key).is_some_and(|new| new >= old))
}
pub fn activity_attempt(proof: &ActivityProof) -> AttemptId {
    proof.inner.attempt
}
