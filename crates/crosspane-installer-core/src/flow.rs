use crate::*;
use crosspane_types::id::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

type Scope = (NodeId, Option<NodeId>);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSpec {
    pub id: StepId,
    pub prerequisites: Vec<StepId>,
    pub required_for_installed: bool,
    pub required_for_ready: bool,
    pub requires_fresh_observation: bool,
    pub requires_activity: bool,
    pub requires_human: bool,
    pub requires_fixture: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepState {
    NotChecked,
    Checking,
    Planning,
    NeedsAction,
    Running,
    Verifying,
    WaitingForUser,
    WaitingForPeer,
    PendingContract,
    Satisfied,
    /// A deliberate deferral, carrying no verification evidence.
    Skipped,
    Stale,
    Failed,
    Unsupported,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStage {
    Detect,
    Plan,
    Apply,
    Verify,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobIntent {
    pub step: StepId,
    pub operation: OperationId,
    pub stage: JobStage,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    pub source: ObservationSource,
    pub observed_at_ms: u64,
    pub binding: Option<EvidenceBinding>,
    pub activity: Option<ActivityProof>,
    pub human_attempt: Option<AttemptId>,
    pub fixture_attempt: Option<AttemptId>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApplyOutcome {
    Applied,
    Refused,
    Failed,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitKind {
    User,
    Peer,
    Contract,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlowEvent {
    Observe {
        samples: Vec<CounterSample>,
    },
    Begin {
        step: StepId,
    },
    Detected {
        step: StepId,
        operation: OperationId,
        needs_action: bool,
    },
    Planned {
        step: StepId,
        operation: OperationId,
    },
    ApplyRequested {
        step: StepId,
        operation: OperationId,
    },
    Applied {
        step: StepId,
        operation: OperationId,
        outcome: ApplyOutcome,
    },
    Verified {
        step: StepId,
        operation: OperationId,
        verification: Verification,
    },
    Waiting {
        step: StepId,
        operation: OperationId,
        kind: WaitKind,
    },
    Failed {
        step: StepId,
        operation: OperationId,
    },
    Unsupported {
        step: StepId,
        operation: OperationId,
    },
    Invalidate {
        step: StepId,
    },
    Cancel {
        step: StepId,
    },
    Skip(StepId),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    #[error("step graph is empty")]
    Empty,
    #[error("duplicate step id")]
    DuplicateStep,
    #[error("unknown prerequisite")]
    UnknownPrerequisite,
    #[error("step graph contains a cycle")]
    Cycle,
    #[error("readiness requires a fresh health gate")]
    NoReadinessFreshnessGate,
    #[error("step evidence requirements conflict")]
    InvalidStepRequirement,
    #[error("only Connect and Arrange can be optional")]
    InvalidOptionalStep,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FlowError {
    #[error("unknown step")]
    UnknownStep,
    #[error("a prerequisite is pending")]
    PrerequisitePending,
    #[error("step is busy")]
    Busy,
    #[error("job result or consent is for a retired operation")]
    WrongOperation,
    #[error("job stage does not match")]
    WrongStage,
    #[error("evidence is invalid")]
    InvalidEvidence,
    #[error("operation ids exhausted")]
    OperationExhausted,
    #[error("this step cannot be skipped")]
    NotOptional,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Milestone {
    NotInstalled,
    InstalledWaiting,
    WorkspaceReady,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSummary {
    pub id: StepId,
    pub state: StepState,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub milestone: Milestone,
    pub steps: Vec<StepSummary>,
    #[serde(default)]
    pub skipped: Vec<StepId>,
}
#[derive(Debug)]
struct Step {
    spec: StepSpec,
    state: StepState,
    job: Option<JobIntent>,
    evidence: Option<Verification>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct NodeAdmission {
    epochs: Epochs,
    observed_at_ms: u64,
}
#[derive(Debug)]
pub struct Flow {
    steps: BTreeMap<StepId, Step>,
    ancestors: BTreeMap<StepId, BTreeSet<StepId>>,
    samples: BTreeMap<Scope, CounterSample>,
    present: BTreeSet<Scope>,
    broken: BTreeSet<(NodeId, u64)>,
    admission: BTreeMap<NodeId, NodeAdmission>,
    retired_instances: BTreeSet<(NodeId, u64)>,
    next_operation: u64,
    last_now: u64,
    optional: BTreeSet<StepId>,
}

impl Flow {
    pub fn new(steps: Vec<StepSpec>) -> Result<Self, GraphError> {
        Self::new_with_optional_steps(steps, &[])
    }

    /// Opt-in only for the shared Connect/Arrange ids. Native installation,
    /// permissions and the final fresh-health gate can never acquire skip authority.
    pub fn new_with_optional_steps(
        steps: Vec<StepSpec>,
        optional_ids: &[StepId],
    ) -> Result<Self, GraphError> {
        if steps.is_empty() {
            return Err(GraphError::Empty);
        }
        let mut map = BTreeMap::new();
        for spec in steps {
            if (spec.requires_fresh_observation
                && (spec.requires_activity || !spec.required_for_ready))
                || (!spec.requires_activity && (spec.requires_human || spec.requires_fixture))
            {
                return Err(GraphError::InvalidStepRequirement);
            }
            let id = spec.id;
            if map
                .insert(
                    id,
                    Step {
                        spec,
                        state: StepState::NotChecked,
                        job: None,
                        evidence: None,
                    },
                )
                .is_some()
            {
                return Err(GraphError::DuplicateStep);
            }
        }
        if map
            .values()
            .any(|s| s.spec.prerequisites.iter().any(|id| !map.contains_key(id)))
        {
            return Err(GraphError::UnknownPrerequisite);
        }
        let mut ancestors: BTreeMap<StepId, BTreeSet<StepId>> = BTreeMap::new();
        while ancestors.len() < map.len() {
            let ready: Vec<_> = map
                .values()
                .filter(|s| {
                    !ancestors.contains_key(&s.spec.id)
                        && s.spec
                            .prerequisites
                            .iter()
                            .all(|id| ancestors.contains_key(id))
                })
                .map(|s| s.spec.id)
                .collect();
            if ready.is_empty() {
                return Err(GraphError::Cycle);
            }
            for id in ready {
                let Some(step) = map.get(&id) else {
                    return Err(GraphError::UnknownPrerequisite);
                };
                let mut closure = BTreeSet::new();
                for parent in &step.spec.prerequisites {
                    closure.insert(*parent);
                    if let Some(upstream) = ancestors.get(parent) {
                        closure.extend(upstream);
                    }
                }
                ancestors.insert(id, closure);
            }
        }
        if !map
            .values()
            .any(|s| s.spec.required_for_ready && s.spec.requires_fresh_observation)
        {
            return Err(GraphError::NoReadinessFreshnessGate);
        }
        let optional: BTreeSet<_> = optional_ids.iter().copied().collect();
        if optional.iter().any(|id| {
            !matches!(id.0, 60..=62)
                || !map.get(id).is_some_and(|step| {
                    !step.spec.required_for_installed && !step.spec.requires_fresh_observation
                })
        }) {
            return Err(GraphError::InvalidOptionalStep);
        }
        Ok(Self {
            steps: map,
            ancestors,
            samples: BTreeMap::new(),
            present: BTreeSet::new(),
            broken: BTreeSet::new(),
            admission: BTreeMap::new(),
            retired_instances: BTreeSet::new(),
            next_operation: 1,
            last_now: 0,
            optional,
        })
    }

    fn retire(&mut self, roots: &BTreeSet<StepId>, state: StepState) {
        for (id, step) in &mut self.steps {
            // Evidence changes cannot undo a person's deferral. Begin explicitly reopens it.
            if step.state == StepState::Skipped {
                continue;
            }
            if roots.contains(id)
                || self
                    .ancestors
                    .get(id)
                    .is_some_and(|a| !a.is_disjoint(roots))
            {
                step.state = state;
                step.job = None;
                step.evidence = None;
            }
        }
    }
    fn evidence_valid(
        &self,
        step: &Step,
        now: u64,
        current: &BTreeMap<Scope, &CounterSample>,
    ) -> bool {
        let Some(v) = &step.evidence else {
            return false;
        };
        if v.source != ObservationSource::Live || v.observed_at_ms > now {
            return false;
        }
        if step.spec.requires_fresh_observation && now - v.observed_at_ms > 5000 {
            return false;
        }
        if step.spec.requires_activity != v.activity.is_some() {
            return false;
        }
        if let Some(proof) = &v.activity {
            if v.human_attempt
                .is_some_and(|a| a != activity_attempt(proof))
                || v.fixture_attempt
                    .is_some_and(|a| a != activity_attempt(proof))
                || v.binding.as_ref() != Some(&proof.end().binding)
                || v.observed_at_ms != proof.end().observed_at_ms
                || (step.spec.requires_human && v.human_attempt != Some(activity_attempt(proof)))
                || (step.spec.requires_fixture
                    && v.fixture_attempt != Some(activity_attempt(proof)))
            {
                return false;
            }
        } else if v.human_attempt.is_some() || v.fixture_attempt.is_some() {
            return false;
        }
        match &v.binding {
            Some(binding) => {
                let scope = binding.scope();
                let Some(sample) = current.get(&scope) else {
                    return false;
                };
                if !self.present.contains(&scope)
                    || self.samples.get(&scope) != Some(*sample)
                    || sample.source != ObservationSource::Live
                    || sample.observed_at_ms > now
                    || sample.observed_at_ms < v.observed_at_ms
                {
                    return false;
                }
                if v.activity.is_some()
                    && self
                        .broken
                        .contains(&(binding.local_node, binding.epochs.instance_id))
                {
                    return false;
                }
                match &v.activity {
                    Some(proof) => activity_is_current(proof, sample),
                    None => binding == &sample.binding,
                }
            }
            None => !step.spec.requires_fresh_observation && v.activity.is_none(),
        }
    }
    fn valid_steps(&self, now: u64, current: &BTreeMap<Scope, &CounterSample>) -> BTreeSet<StepId> {
        let own: BTreeSet<_> = self
            .steps
            .iter()
            .filter(|(_, step)| {
                step.state == StepState::Satisfied && self.evidence_valid(step, now, current)
            })
            .map(|(id, _)| *id)
            .collect();
        own.iter()
            .copied()
            .filter(|id| {
                self.ancestors.get(id).is_some_and(|a| {
                    a.iter()
                        .all(|parent| own.contains(parent) || self.skipped(*parent))
                })
            })
            .collect()
    }

    fn skipped(&self, id: StepId) -> bool {
        self.optional.contains(&id)
            && self
                .steps
                .get(&id)
                .is_some_and(|s| s.state == StepState::Skipped)
    }
    fn refresh(&mut self, now: u64) {
        let current = self
            .samples
            .iter()
            .filter(|(scope, _)| self.present.contains(scope))
            .map(|(scope, s)| (*scope, s))
            .collect();
        let valid = self.valid_steps(now, &current);
        let invalid = self
            .steps
            .iter()
            .filter(|(id, s)| s.state == StepState::Satisfied && !valid.contains(id))
            .map(|(id, _)| *id)
            .collect();
        self.retire(&invalid, StepState::Stale);
    }
    fn observe(&mut self, samples: Vec<CounterSample>, now: u64) -> Result<(), FlowError> {
        let mut next = BTreeMap::new();
        let mut invalid = now < self.last_now;
        let mut regressions = BTreeSet::new();
        let mut instances = BTreeMap::new();
        for sample in samples {
            let scope = sample.binding.scope();
            let epochs = &sample.binding.epochs;
            invalid |= sample.source != ObservationSource::Live
                || sample.observed_at_ms > now
                || !sample.binding.well_formed();
            let node = NodeAdmission {
                epochs: epochs.clone(),
                observed_at_ms: sample.observed_at_ms,
            };
            if let Some(prior) = instances.insert(scope.0, node.clone()) {
                invalid |= prior != node;
            }
            // Instance/time/epoch admission is node-wide, not tied to the selected peer.
            invalid |= self
                .retired_instances
                .contains(&(scope.0, epochs.instance_id));
            if let Some(prior) = self.admission.get(&scope.0) {
                invalid |= sample.observed_at_ms < prior.observed_at_ms;
                if prior.epochs.instance_id == epochs.instance_id {
                    invalid |= prior.epochs.gate_epoch > epochs.gate_epoch
                        || prior.epochs.grants_epoch > epochs.grants_epoch
                        || prior.epochs.layout_epoch > epochs.layout_epoch
                        || prior.epochs.backends_epoch > epochs.backends_epoch;
                }
            }
            if let Some(old) = self.samples.get(&scope) {
                invalid |= sample.observed_at_ms < old.observed_at_ms;
                if old.binding.epochs.instance_id == epochs.instance_id {
                    let previous = &old.binding.epochs;
                    let decreased = previous.gate_epoch > epochs.gate_epoch
                        || previous.grants_epoch > epochs.grants_epoch
                        || previous.layout_epoch > epochs.layout_epoch
                        || previous.backends_epoch > epochs.backends_epoch
                        || old.values.iter().any(|(key, value)| {
                            sample.values.get(key).is_some_and(|new| new < value)
                        });
                    invalid |= old
                        .values
                        .keys()
                        .any(|key| !sample.values.contains_key(key));
                    if decreased {
                        regressions.insert((scope.0, epochs.instance_id));
                        invalid = true;
                    }
                }
            }
            invalid |= next.insert(scope, sample).is_some();
        }
        // Taints survive rejected/mixed batches and every later instance transition.
        self.broken.extend(regressions.iter().copied());
        if invalid {
            let affected = self
                .steps
                .iter()
                .filter(|(_, s)| {
                    s.spec.requires_activity
                        || s.evidence.as_ref().is_some_and(|v| v.binding.is_some())
                })
                .map(|(id, _)| *id)
                .collect();
            self.retire(&affected, StepState::Stale);
            if !regressions.is_empty() {
                for step in self.steps.values_mut().filter(|s| s.spec.requires_activity) {
                    step.state = StepState::PendingContract;
                }
            }
            return Err(FlowError::InvalidEvidence);
        }
        for (node, observed) in instances {
            if let Some(prior) = self.admission.get(&node)
                && prior.epochs.instance_id != observed.epochs.instance_id
            {
                self.retired_instances
                    .insert((node, prior.epochs.instance_id));
            }
            self.admission.insert(node, observed);
        }
        // One last sample per scope, including temporarily absent scopes; presence is separate.
        self.present = next.keys().copied().collect();
        self.samples.extend(next);
        self.refresh(now);
        Ok(())
    }
    fn check_job(
        &self,
        step: StepId,
        operation: OperationId,
        stage: Option<JobStage>,
    ) -> Result<(), FlowError> {
        let s = self.steps.get(&step).ok_or(FlowError::UnknownStep)?;
        let job = s.job.as_ref().ok_or(FlowError::WrongOperation)?;
        if job.operation != operation {
            return Err(FlowError::WrongOperation);
        }
        if stage.is_some_and(|wanted| wanted != job.stage) {
            return Err(FlowError::WrongStage);
        }
        Ok(())
    }
    fn schedule(&mut self, step: StepId, stage: JobStage) -> Result<Vec<JobIntent>, FlowError> {
        let next = self
            .next_operation
            .checked_add(1)
            .ok_or(FlowError::OperationExhausted)?;
        let job = JobIntent {
            step,
            operation: OperationId(self.next_operation),
            stage,
        };
        let s = self.steps.get_mut(&step).ok_or(FlowError::UnknownStep)?;
        self.next_operation = next;
        s.state = match stage {
            JobStage::Detect => StepState::Checking,
            JobStage::Plan => StepState::Planning,
            JobStage::Apply => StepState::Running,
            JobStage::Verify => StepState::Verifying,
        };
        s.job = Some(job.clone());
        s.evidence = None;
        Ok(vec![job])
    }
    pub fn reduce(&mut self, event: FlowEvent, now_ms: u64) -> Result<Vec<JobIntent>, FlowError> {
        if now_ms < self.last_now {
            self.retire(&self.steps.keys().copied().collect(), StepState::Stale);
            if let FlowEvent::Observe { samples } = event {
                // Rejected clock changes must still retain every observed regression taint.
                return self.observe(samples, now_ms).map(|()| Vec::new());
            }
            return Err(FlowError::InvalidEvidence);
        }
        self.last_now = now_ms;
        if let FlowEvent::Observe { samples } = event {
            self.observe(samples, now_ms)?;
            return Ok(Vec::new());
        }
        self.refresh(now_ms);
        let (id, operation, stage) = match &event {
            FlowEvent::Skip(step) => (*step, None, None),
            FlowEvent::Begin { step }
            | FlowEvent::Cancel { step }
            | FlowEvent::Invalidate { step } => (*step, None, None),
            FlowEvent::Detected {
                step, operation, ..
            } => (*step, Some(*operation), Some(JobStage::Detect)),
            FlowEvent::Planned { step, operation }
            | FlowEvent::ApplyRequested { step, operation } => {
                (*step, Some(*operation), Some(JobStage::Plan))
            }
            FlowEvent::Applied {
                step, operation, ..
            } => (*step, Some(*operation), Some(JobStage::Apply)),
            FlowEvent::Verified {
                step, operation, ..
            } => (*step, Some(*operation), Some(JobStage::Verify)),
            FlowEvent::Waiting {
                step, operation, ..
            }
            | FlowEvent::Failed { step, operation }
            | FlowEvent::Unsupported { step, operation } => (*step, Some(*operation), None),
            FlowEvent::Observe { .. } => return Err(FlowError::WrongStage),
        };
        let step = self.steps.get(&id).ok_or(FlowError::UnknownStep)?;
        if let Some(operation) = operation {
            self.check_job(id, operation, stage)?;
        }
        if !matches!(
            event,
            FlowEvent::Cancel { .. } | FlowEvent::Invalidate { .. } | FlowEvent::Skip(_)
        ) {
            let current = self
                .samples
                .iter()
                .filter(|(scope, _)| self.present.contains(scope))
                .map(|(scope, s)| (*scope, s))
                .collect();
            let valid = self.valid_steps(now_ms, &current);
            if !self.ancestors.get(&id).is_some_and(|a| {
                a.iter()
                    .all(|parent| valid.contains(parent) || self.skipped(*parent))
            }) {
                return Err(FlowError::PrerequisitePending);
            }
        }
        match event {
            FlowEvent::Skip(_) => {
                if !self.optional.contains(&id) {
                    return Err(FlowError::NotOptional);
                }
                self.retire(&BTreeSet::from([id]), StepState::Stale);
                let step = self.steps.get_mut(&id).ok_or(FlowError::UnknownStep)?;
                step.state = StepState::Skipped;
                step.job = None;
                step.evidence = None;
            }
            FlowEvent::Begin { .. } => {
                if matches!(
                    step.state,
                    StepState::Checking
                        | StepState::Planning
                        | StepState::Running
                        | StepState::Verifying
                ) {
                    return Err(FlowError::Busy);
                }
                self.steps.get_mut(&id).ok_or(FlowError::UnknownStep)?.state = StepState::Stale;
                self.retire(&BTreeSet::from([id]), StepState::Stale);
                return self.schedule(id, JobStage::Detect);
            }
            FlowEvent::Detected { needs_action, .. } => {
                return self.schedule(
                    id,
                    if needs_action {
                        JobStage::Plan
                    } else {
                        JobStage::Verify
                    },
                );
            }
            FlowEvent::Planned { .. } => {
                if step.state != StepState::Planning {
                    return Err(FlowError::WrongStage);
                }
                self.steps.get_mut(&id).ok_or(FlowError::UnknownStep)?.state =
                    StepState::NeedsAction;
            }
            FlowEvent::ApplyRequested { .. } => {
                if step.state != StepState::NeedsAction {
                    return Err(FlowError::WrongStage);
                }
                return self.schedule(id, JobStage::Apply);
            }
            FlowEvent::Applied { outcome, .. } => match outcome {
                ApplyOutcome::Applied => return self.schedule(id, JobStage::Verify),
                ApplyOutcome::Unknown => return self.schedule(id, JobStage::Detect),
                ApplyOutcome::Refused | ApplyOutcome::Failed => {
                    let state = if outcome == ApplyOutcome::Refused {
                        StepState::WaitingForUser
                    } else {
                        StepState::Failed
                    };
                    self.retire(&BTreeSet::from([id]), state);
                }
            },
            FlowEvent::Verified { verification, .. } => {
                let current = self
                    .samples
                    .iter()
                    .filter(|(scope, _)| self.present.contains(scope))
                    .map(|(scope, s)| (*scope, s))
                    .collect();
                let candidate = Step {
                    spec: step.spec.clone(),
                    state: StepState::Satisfied,
                    job: None,
                    evidence: Some(verification),
                };
                if !self.evidence_valid(&candidate, now_ms, &current) {
                    self.retire(&BTreeSet::from([id]), StepState::PendingContract);
                    return Err(FlowError::InvalidEvidence);
                }
                self.steps.insert(id, candidate);
            }
            FlowEvent::Waiting { kind, .. } => self.retire(
                &BTreeSet::from([id]),
                match kind {
                    WaitKind::User => StepState::WaitingForUser,
                    WaitKind::Peer => StepState::WaitingForPeer,
                    WaitKind::Contract => StepState::PendingContract,
                },
            ),
            FlowEvent::Failed { .. } => self.retire(&BTreeSet::from([id]), StepState::Failed),
            FlowEvent::Unsupported { .. } => {
                self.retire(&BTreeSet::from([id]), StepState::Unsupported)
            }
            FlowEvent::Cancel { .. } | FlowEvent::Invalidate { .. } => {
                self.retire(&BTreeSet::from([id]), StepState::Stale)
            }
            FlowEvent::Observe { .. } => return Err(FlowError::WrongStage),
        }
        Ok(Vec::new())
    }
    pub fn summary(&self, now_ms: u64, current: &[CounterSample]) -> Summary {
        let map: BTreeMap<_, _> = current.iter().map(|s| (s.binding.scope(), s)).collect();
        let valid = if map.len() != current.len() || now_ms < self.last_now {
            BTreeSet::new()
        } else {
            self.valid_steps(now_ms, &map)
        };
        let installed = self
            .steps
            .values()
            .filter(|s| s.spec.required_for_installed)
            .all(|s| valid.contains(&s.spec.id));
        let ready = installed
            && self
                .steps
                .values()
                .filter(|s| s.spec.required_for_ready)
                .all(|s| valid.contains(&s.spec.id) || self.skipped(s.spec.id));
        Summary {
            skipped: self
                .steps
                .keys()
                .copied()
                .filter(|id| self.skipped(*id))
                .collect(),
            milestone: if ready {
                Milestone::WorkspaceReady
            } else if installed {
                Milestone::InstalledWaiting
            } else {
                Milestone::NotInstalled
            },
            steps: self
                .steps
                .iter()
                .map(|(id, s)| StepSummary {
                    id: *id,
                    state: if s.state == StepState::Satisfied && !valid.contains(id) {
                        StepState::Stale
                    } else {
                        s.state
                    },
                })
                .collect(),
        }
    }
}
