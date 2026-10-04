//! The readiness graph: platform steps first, then the shared steps. Flags are `StepSpec` data;
//! core alone computes milestones from them.

use std::collections::BTreeSet;

use crosspane_installer_core::{StepId, StepSpec};

use super::{LiveError, PlatformDescription};
use crate::tutorial_flow::TutorialRole;
use crate::view::{ProgressGroup, ScreenId};

pub mod steps {
    use crosspane_installer_core::StepId;

    pub const PAIR: StepId = StepId(60);
    pub const GRANTS: StepId = StepId(61);
    pub const LAYOUT: StepId = StepId(62);
    pub const HIDING: StepId = StepId(63);
    pub const FINAL: StepId = StepId(90);

    /// The practice step for one of the nine roles.
    pub fn practice(role: crate::tutorial_flow::TutorialRole) -> StepId {
        StepId(70 + super::role_index(role) as u16)
    }
}

pub const ROLES: [TutorialRole; 9] = [
    TutorialRole::E1Controller,
    TutorialRole::E1Target,
    TutorialRole::E2SourcePush,
    TutorialRole::E2DestinationPush,
    TutorialRole::E2SourcePull,
    TutorialRole::E2DestinationPull,
    TutorialRole::AudioSender,
    TutorialRole::AudioReceiver,
    TutorialRole::Menu,
];

pub fn role_index(role: TutorialRole) -> usize {
    ROLES.iter().position(|r| *r == role).unwrap_or(0)
}

pub fn role_of(step: StepId) -> Option<TutorialRole> {
    step.0
        .checked_sub(70)
        .and_then(|i| ROLES.get(usize::from(i)).copied())
}

/// The sequencer's fixture-owning roles; their proofs carry the fixture attempt.
pub fn uses_fixture(role: TutorialRole) -> bool {
    matches!(
        role,
        TutorialRole::E1Target
            | TutorialRole::E2SourcePush
            | TutorialRole::E2SourcePull
            | TutorialRole::AudioSender
    )
}

pub fn role_label(role: TutorialRole) -> &'static str {
    match role {
        TutorialRole::E1Controller => "Control the other computer from this keyboard and mouse",
        TutorialRole::E1Target => "Let the other computer control this one",
        TutorialRole::E2SourcePush => "Send a window from this computer",
        TutorialRole::E2DestinationPush => "Receive a window sent from the other computer",
        TutorialRole::E2SourcePull => "Let the other computer take a window from here",
        TutorialRole::E2DestinationPull => "Take a window from the other computer",
        TutorialRole::AudioSender => "Play sound from this computer on the other one",
        TutorialRole::AudioReceiver => "Hear the other computer's sound here",
        TutorialRole::Menu => "Find the Crosspane menu and settings",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    Native,
    Pair,
    Grants,
    Layout,
    Hiding,
    Practice(TutorialRole),
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
    pub fn practice_steps(&self) -> Vec<StepId> {
        ROLES.iter().map(|r| steps::practice(*r)).collect()
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
        || !known(&desc.practice_after)
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
        let role = match kind {
            StepKind::Practice(role) => Some(role),
            _ => None,
        };
        specs.push(StepSpec {
            id,
            prerequisites: prerequisites.clone(),
            required_for_installed: false,
            required_for_ready: true,
            requires_fresh_observation: kind == StepKind::Final,
            requires_activity: role.is_some(),
            requires_human: role.is_some(),
            requires_fixture: role.is_some_and(uses_fixture),
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
    let mut practice_after = desc.practice_after.clone();
    if desc.hiding_choice {
        shared(
            steps::HIDING,
            StepKind::Hiding,
            ScreenId::HidingChoice,
            ProgressGroup::PermissionsNetwork,
            "How windows you send are hidden here",
            desc.connect_after.clone(),
        );
        practice_after.push(steps::HIDING);
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
    let mut before_practice = vec![steps::GRANTS, steps::LAYOUT];
    before_practice.extend(practice_after);
    before_practice.sort();
    before_practice.dedup();
    for role in ROLES {
        shared(
            steps::practice(role),
            StepKind::Practice(role),
            ScreenId::Practice,
            ProgressGroup::Practice,
            role_label(role),
            before_practice.clone(),
        );
    }
    shared(
        steps::FINAL,
        StepKind::Final,
        ScreenId::Summary,
        ProgressGroup::Ready,
        "Everything is working right now",
        ROLES.iter().map(|r| steps::practice(*r)).collect(),
    );
    Ok(Graph { metas, specs })
}
