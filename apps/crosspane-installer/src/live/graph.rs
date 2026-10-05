//! The readiness graph: platform steps first, then the shared steps. Flags are `StepSpec` data;
//! core alone computes milestones from them.

use std::collections::BTreeSet;

use crosspane_installer_core::{StepId, StepSpec};

use super::{LiveError, PlatformDescription};
use crate::view::{ProgressGroup, ScreenId};

pub mod steps {
    use crosspane_installer_core::StepId;

    pub const PAIR: StepId = StepId(60);
    pub const GRANTS: StepId = StepId(61);
    pub const LAYOUT: StepId = StepId(62);
    pub const HIDING: StepId = StepId(63);
    pub const FINAL: StepId = StepId(90);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    Native,
    Pair,
    Grants,
    Layout,
    Hiding,
    Final,
}

#[derive(Clone, Debug)]
pub struct StepMeta {
    pub id: StepId,
    pub kind: StepKind,
    pub screen: ScreenId,
    pub group: ProgressGroup,
    pub label: String,
    pub action_label: String,
    pub prerequisites: Vec<StepId>,
    pub uses_status: bool,
    pub settles_with_peer: bool,
    pub agent_apply: Option<super::AgentApply>,
}

#[derive(Debug)]
pub struct Graph {
    pub metas: Vec<StepMeta>,
    pub specs: Vec<StepSpec>,
}

impl Graph {
    pub fn meta(&self, step: StepId) -> Option<&StepMeta> {
        self.metas.iter().find(|m| m.id == step)
    }
    pub fn kind(&self, step: StepId) -> Option<StepKind> {
        self.meta(step).map(|m| m.kind)
    }
    pub fn on_screen(&self, screen: ScreenId) -> impl Iterator<Item = &StepMeta> {
        self.metas.iter().filter(move |m| m.screen == screen)
    }
}

pub fn build(desc: &PlatformDescription) -> Result<Graph, LiveError> {
    let mut native = BTreeSet::new();
    for step in &desc.steps {
        if !(10..=59).contains(&step.id.0) || !native.insert(step.id) {
            return Err(LiveError::StepIds);
        }
    }
    let known = |ids: &[StepId]| ids.iter().all(|id| native.contains(id));
    if !desc.steps.iter().all(|s| known(&s.prerequisites))
        || !known(&desc.connect_after)
        || !known(&desc.ready_after)
    {
        return Err(LiveError::UnknownAnchor);
    }
    let mut metas: Vec<StepMeta> = desc
        .steps
        .iter()
        .map(|s| StepMeta {
            id: s.id,
            kind: StepKind::Native,
            screen: s.screen,
            group: s.group,
            label: s.label.clone(),
            action_label: s.action_label.clone(),
            prerequisites: s.prerequisites.clone(),
            uses_status: s.uses_status,
            settles_with_peer: s.settles_with_peer,
            agent_apply: s.agent_apply,
        })
        .collect();
    let mut specs: Vec<StepSpec> = desc
        .steps
        .iter()
        .map(|s| StepSpec {
            id: s.id,
            prerequisites: s.prerequisites.clone(),
            required_for_installed: s.required_for_installed,
            required_for_ready: s.required_for_ready || s.required_for_installed,
            requires_fresh_observation: false,
            requires_activity: false,
            requires_human: false,
            requires_fixture: false,
        })
        .collect();
    let mut shared = |id: StepId,
                      kind: StepKind,
                      screen: ScreenId,
                      group: ProgressGroup,
                      label: &str,
                      prerequisites: Vec<StepId>| {
        specs.push(StepSpec {
            id,
            prerequisites: prerequisites.clone(),
            required_for_installed: false,
            required_for_ready: true,
            requires_fresh_observation: kind == StepKind::Final,
            requires_activity: false,
            requires_human: false,
            requires_fixture: false,
        });
        metas.push(StepMeta {
            id,
            kind,
            screen,
            group,
            label: label.into(),
            action_label: String::new(),
            prerequisites,
            uses_status: false,
            settles_with_peer: false,
            agent_apply: None,
        });
    };
    let mut ready_after = desc.ready_after.clone();
    if desc.hiding_choice {
        shared(
            steps::HIDING,
            StepKind::Hiding,
            ScreenId::HidingChoice,
            ProgressGroup::PermissionsNetwork,
            "How windows you send are hidden here",
            desc.connect_after.clone(),
        );
        ready_after.push(steps::HIDING);
    }
    shared(
        steps::PAIR,
        StepKind::Pair,
        ScreenId::Connect,
        ProgressGroup::Connect,
        "Paired and connected to the other computer",
        desc.connect_after.clone(),
    );
    shared(
        steps::GRANTS,
        StepKind::Grants,
        ScreenId::Grants,
        ProgressGroup::Arrange,
        "What the other computer may do here",
        vec![steps::PAIR],
    );
    shared(
        steps::LAYOUT,
        StepKind::Layout,
        ScreenId::Layout,
        ProgressGroup::Arrange,
        "Where the other computer's screens sit",
        vec![steps::PAIR],
    );
    let mut before_ready = vec![steps::GRANTS, steps::LAYOUT];
    before_ready.extend(ready_after);
    before_ready.sort();
    before_ready.dedup();
    shared(
        steps::FINAL,
        StepKind::Final,
        ScreenId::Summary,
        ProgressGroup::Ready,
        "Everything is working right now",
        before_ready,
    );
    Ok(Graph { metas, specs })
}
