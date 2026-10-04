//! Agent-driven shared steps: pairing and reconnection, explicit grants, committed layout, the
//! macOS hiding choice and final health. Acknowledgements never verify; each step is verified
//! only from a Status reply issued after its Verify job started.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use crosspane_installer_core::{
    ApplyOutcome, FlowEvent, JobIntent, JobStage, StepId, StepState, Verification, WaitKind,
};
use crosspane_types::id::NodeId;
use crosspane_ui_kit::layout::{DisplayRect, LayoutAction};

use super::controller::{LiveController, OwnCall, bounded};
use super::graph::steps;
use crate::agent_contract::{
    AgentReply, BackendName, BackendState, CallFailure, Capability, DecodedReply,
    GrantableCapability, HealthSnapshot, InstallerRequest, KeyStoreProvenance, PairCandidate,
    PairPhase, PairingStatus, Placement, SessionState, StartupRecovery,
};
use crate::gui::ShellEffect;
use crate::view::HidingChoice;

pub(super) const CAPABILITIES: [GrantableCapability; 5] = [
    GrantableCapability::Input,
    GrantableCapability::Share,
    GrantableCapability::Browse,
    GrantableCapability::Present,
    GrantableCapability::Speaker,
];
const PAIR_POLL_MS: u64 = 500;
const PAIR_TIMEOUT_MS: u64 = 180_000;
const VERIFY_TIMEOUT_MS: u64 = 20_000;
const MAX_CANDIDATES: usize = 8;
const MAX_ADDRESS: usize = 256;

pub(super) fn capability_label(c: GrantableCapability) -> &'static str {
    match c {
        GrantableCapability::Input => "Control this computer's keyboard and mouse",
        GrantableCapability::Share => "Receive windows sent from this computer",
        GrantableCapability::Browse => "See this computer's window list and take windows",
        GrantableCapability::Present => "Show its windows on this computer",
        GrantableCapability::Speaker => "Play its sound on this computer's speakers",
    }
}

fn capability(c: GrantableCapability) -> Capability {
    match c {
        GrantableCapability::Input => Capability::Input,
        GrantableCapability::Share => Capability::Share,
        GrantableCapability::Browse => Capability::Browse,
        GrantableCapability::Present => Capability::Present,
        GrantableCapability::Speaker => Capability::Speaker,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum PairMode {
    Listen,
    Join(SocketAddr),
    Dial(SocketAddr),
    Existing(NodeId),
}

#[derive(Default)]
pub(super) struct ConnectState {
    pub address: String,
    pub candidates: Vec<PairCandidate>,
    pub pairing: Option<PairingStatus>,
    pub mode: Option<PairMode>,
    pub known: BTreeSet<NodeId>,
    pub polling: bool,
    pub poll_at: u64,
    pub started_at: u64,
    /// Detection after an unknown outcome must read a Status issued after it.
    pub detect_floor: BTreeMap<StepId, u64>,
    /// (first admissible Status call id, Verify start time).
    pub verify_from: BTreeMap<StepId, (u64, u64)>,
    pub grants: [bool; 5],
    pub grants_pending: BTreeSet<u64>,
    pub grants_outcome: Option<ApplyOutcome>,
    pub place: Option<Vec<Placement>>,
    pub placed: Option<Vec<Placement>>,
    pub layout_busy: bool,
    pub hiding: Option<HidingChoice>,
    pub settings: Option<crate::tutorial_flow::SettingsTransition>,
}

pub(super) fn outcome(failure: &CallFailure) -> ApplyOutcome {
    match failure {
        CallFailure::Refused(_) => ApplyOutcome::Refused,
        CallFailure::TimeoutOutcomeUnknown => ApplyOutcome::Unknown,
        _ => ApplyOutcome::Failed,
    }
}

pub(super) fn failure_text(failure: &CallFailure) -> &'static str {
    match failure {
        CallFailure::Refused(_) => "The agent refused this request.",
        CallFailure::TimeoutOutcomeUnknown => {
            "The agent didn't answer in time, so the result is unknown. Checking again."
        }
        CallFailure::Unavailable => "The agent isn't reachable.",
        _ => "The agent gave an unexpected answer.",
    }
}

/// The final health gate: every role's safety and backend facts, plus the selected peer.
fn final_healthy(health: &HealthSnapshot, peer: Option<NodeId>) -> Result<(), &'static str> {
    let i = health.installer();
    if !i.gate.open || i.gate.panic || i.gate.active != Some(true) {
        return Err("Crosspane's input gate isn't open on this computer.");
    }
    if i.gate.session != SessionState::Unlocked {
        return Err("This computer's session is locked or its lock state is unknown.");
    }
    if i.keystore != KeyStoreProvenance::OsStore {
        return Err("Crosspane's key isn't held by the system keyring.");
    }
    if i.startup_recovery == StartupRecovery::Failed || i.recovery_pending != 0 {
        return Err("Crosspane is still recovering windows from an earlier session.");
    }
    use BackendName::*;
    let needed = [
        Capture, Keys, Pointer, Overlay, Hotkeys, Keystore, Windows, Parking, Frames, Gpu, Links,
        Audio, Tray,
    ];
    if !needed.iter().all(|n| {
        i.backends
            .iter()
            .any(|b| b.name == *n && b.state == BackendState::Ready)
    }) {
        return Err("A part of Crosspane isn't ready on this computer.");
    }
    let Some(peer) = peer else {
        return Err("No paired computer is selected.");
    };
    if !i.peers.iter().any(|p| p.node == peer && p.connected) {
        return Err("The other computer isn't connected.");
    }
    Ok(())
}

impl LiveController {
    fn health_ref(&self) -> Option<&HealthSnapshot> {
        self.health.as_ref().map(|h| h.snapshot.as_ref())
    }

    fn connected(&self, node: NodeId) -> bool {
        self.health_ref().is_some_and(|h| {
            h.installer()
                .peers
                .iter()
                .any(|p| p.node == node && p.connected)
        })
    }

    pub(super) fn connected_peers(&self) -> Vec<(NodeId, String)> {
        self.health_ref()
            .map(|h| {
                h.installer()
                    .peers
                    .iter()
                    .filter(|p| p.connected)
                    .map(|p| (p.node, p.name.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn peer_name(&self) -> Option<String> {
        let peer = self.peer?;
        self.health_ref()?
            .installer()
            .peers
            .iter()
            .find(|p| p.node == peer)
            .map(|p| bounded(p.name.clone()))
    }

    /// The selected peer, or the single connected one.
    fn current_peer(&self) -> Option<NodeId> {
        if let Some(peer) = self.peer
            && self.connected(peer)
        {
            return Some(peer);
        }
        let connected = self.connected_peers();
        (connected.len() == 1).then(|| connected[0].0)
    }

    fn granted(&self) -> [bool; 5] {
        let mut out = [false; 5];
        let (Some(h), Some(peer)) = (self.health_ref(), self.peer) else {
            return out;
        };
        if let Some(p) = h.installer().peers.iter().find(|p| p.node == peer) {
            for (i, c) in CAPABILITIES.iter().enumerate() {
                out[i] = p.grants_given.contains(&capability(*c));
            }
        }
        out
    }

    fn layout_committed(&self) -> bool {
        let (Some(h), Some(peer)) = (self.health_ref(), self.peer) else {
            return false;
        };
        let placements = &h.display_layout().placements;
        let local = h.installer().node;
        placements.iter().any(|p| p.node == local) && placements.iter().any(|p| p.node == peer)
    }

    fn placed_matches(&self) -> bool {
        let Some(h) = self.health_ref() else {
            return false;
        };
        let Some(placed) = &self.connect.placed else {
            return true;
        };
        placed.iter().all(|want| {
            h.display_layout().placements.iter().any(|p| {
                p.node == want.node
                    && p.display == want.display
                    && (p.origin_mm[0] - want.origin_mm[0]).abs() < 0.5
                    && (p.origin_mm[1] - want.origin_mm[1]).abs() < 0.5
            })
        })
    }

    pub(super) fn layout_rects(&self) -> (Vec<DisplayRect>, String, Vec<String>) {
        let Some(h) = self.health_ref() else {
            return (Vec::new(), String::new(), Vec::new());
        };
        let local = h.installer().node;
        let layout = h.display_layout();
        let rects = layout
            .placements
            .iter()
            .filter_map(|placement| {
                let (machine, displays) = if placement.node == local {
                    (self.desc.machine_label.clone(), &layout.local_displays)
                } else {
                    let name = h
                        .installer()
                        .peers
                        .iter()
                        .find(|p| p.node == placement.node)
                        .map(|p| bounded(p.name.clone()))?;
                    let displays = &layout
                        .peer_displays
                        .iter()
                        .find(|d| d.node == placement.node)?
                        .displays;
                    (name, displays)
                };
                let display = displays.iter().find(|d| d.id == placement.display);
                let size = display.map_or([0.0; 2], |d| d.mm);
                let size = if size.iter().all(|n| n.is_finite() && *n > 0.0) {
                    size
                } else {
                    [0.0; 2]
                };
                Some(DisplayRect {
                    node: placement.node.to_string(),
                    machine,
                    display: placement.display,
                    name: display.map_or_else(String::new, |d| bounded(d.name.clone())),
                    origin: placement.origin_mm,
                    size,
                    pixels: display.map_or([0; 2], |d| d.pixels),
                })
            })
            .collect();
        let mut peers: Vec<String> = layout
            .placements
            .iter()
            .filter(|p| p.node != local)
            .map(|p| p.node.to_string())
            .collect();
        peers.dedup();
        (rects, local.to_string(), peers)
    }

    fn apply_outcome(&mut self, step: StepId, outcome: ApplyOutcome) {
        let Some(job) = self.job(step, JobStage::Apply) else {
            return;
        };
        if outcome == ApplyOutcome::Unknown {
            self.connect.detect_floor.insert(step, self.next_call);
        }
        let _ = self.reduce(FlowEvent::Applied {
            step,
            operation: job.operation,
            outcome,
        });
    }

    pub(super) fn request_apply(&mut self, step: StepId) {
        if self.step_state(step) != StepState::NeedsAction {
            return;
        }
        let Some(plan) = self.job(step, JobStage::Plan) else {
            return;
        };
        let _ = self.reduce(FlowEvent::ApplyRequested {
            step,
            operation: plan.operation,
        });
        self.dispatch_intents();
    }

    fn start_verify(&mut self, step: StepId) {
        self.connect
            .verify_from
            .insert(step, (self.next_call, self.now));
    }

    /// Detection for agent-driven steps reads the latest Status, or waits for a newer one.
    fn resolve_detect(&mut self, job: &JobIntent) {
        let floor = self
            .connect
            .detect_floor
            .get(&job.step)
            .copied()
            .unwrap_or(0);
        let Some(call) = self.health.as_ref().map(|h| h.call) else {
            // Detection resumes on the next Status that reports; say why it is waiting.
            self.details.insert(
                job.step,
                "Waiting for this computer's Crosspane agent to report.".into(),
            );
            return;
        };
        if call < floor {
            return;
        }
        self.details.remove(&job.step);
        self.connect.detect_floor.remove(&job.step);
        let step = job.step;
        let needs_action = match step {
            steps::PAIR => match self.current_peer() {
                Some(peer) => {
                    self.peer = Some(peer);
                    false
                }
                None => true,
            },
            steps::GRANTS => {
                if self.peer.is_none() {
                    let _ = self.reduce(FlowEvent::Waiting {
                        step,
                        operation: job.operation,
                        kind: WaitKind::Peer,
                    });
                    return;
                }
                self.granted() != [true; 5]
            }
            _ => true,
        };
        let _ = self.reduce(FlowEvent::Detected {
            step,
            operation: job.operation,
            needs_action,
        });
    }

    fn planned(&mut self, job: &JobIntent, preview: &str) {
        self.previews.insert(job.step, preview.into());
        let _ = self.reduce(FlowEvent::Planned {
            step: job.step,
            operation: job.operation,
        });
    }

    pub(super) fn dispatch_pair(&mut self, job: JobIntent) {
        match job.stage {
            JobStage::Detect => self.resolve_detect(&job),
            JobStage::Plan => {
                self.connect.mode = None;
                self.connect.pairing = None;
                self.planned(
                    &job,
                    "Pair this computer with another one running Crosspane. Pairing never lets \
                     the other computer use this keyboard and mouse; you choose that next.",
                );
            }
            JobStage::Apply => self.start_pairing(),
            JobStage::Verify => self.start_verify(job.step),
        }
    }

    fn start_pairing(&mut self) {
        self.connect.known = self
            .health_ref()
            .map(|h| h.installer().peers.iter().map(|p| p.node).collect())
            .unwrap_or_default();
        let (kind, request) = match self.connect.mode.clone() {
            Some(PairMode::Listen) => (
                OwnCall::PairListen,
                InstallerRequest::PairListen { allow_input: false },
            ),
            Some(PairMode::Join(addr)) => (
                OwnCall::PairJoin,
                InstallerRequest::PairJoin {
                    addr,
                    allow_input: false,
                },
            ),
            Some(PairMode::Dial(addr)) => (OwnCall::Dial, InstallerRequest::Dial { addr }),
            Some(PairMode::Existing(node)) => {
                self.peer = Some(node);
                self.apply_outcome(steps::PAIR, ApplyOutcome::Applied);
                return;
            }
            None => {
                self.apply_outcome(steps::PAIR, ApplyOutcome::Failed);
                return;
            }
        };
        self.connect.pairing = None;
        self.connect.started_at = self.now;
        if let Err(failure) = self.call(kind, request) {
            self.details
                .insert(steps::PAIR, failure_text(&failure).into());
            self.apply_outcome(steps::PAIR, outcome(&failure));
        }
    }

    pub(super) fn pair_action(&mut self, mode: PairMode) {
        if self.step_state(steps::PAIR) != StepState::NeedsAction {
            return;
        }
        self.connect.mode = Some(mode);
        self.request_apply(steps::PAIR);
    }

    pub(super) fn parsed_address(&self) -> Option<SocketAddr> {
        self.connect.address.trim().parse().ok()
    }

    pub(super) fn pair_started(&mut self, kind: OwnCall, reply: AgentReply) {
        if self.job(steps::PAIR, JobStage::Apply).is_none() {
            return;
        }
        match reply.result {
            Ok(_) if kind == OwnCall::Dial => {
                self.apply_outcome(steps::PAIR, ApplyOutcome::Applied);
            }
            Ok(_) => {
                self.connect.polling = true;
                self.connect.poll_at = self.now;
            }
            Err(failure) => {
                self.details
                    .insert(steps::PAIR, failure_text(&failure).into());
                self.apply_outcome(steps::PAIR, outcome(&failure));
            }
        }
    }

    pub(super) fn pair_status_reply(&mut self, reply: AgentReply) {
        if self.job(steps::PAIR, JobStage::Apply).is_none() {
            self.connect.polling = false;
            return;
        }
        let Ok(DecodedReply::PairStatus(status)) = reply.result else {
            return;
        };
        let phase = status.phase;
        let error = status.error.clone();
        self.connect.pairing = Some(status);
        match phase {
            PairPhase::Paired => {
                self.connect.polling = false;
                self.apply_outcome(steps::PAIR, ApplyOutcome::Applied);
            }
            PairPhase::Failed => {
                self.connect.polling = false;
                self.details.insert(
                    steps::PAIR,
                    bounded(error.unwrap_or_else(|| "Pairing didn't finish.".into())),
                );
                self.apply_outcome(steps::PAIR, ApplyOutcome::Failed);
            }
            _ => {}
        }
    }

    /// The person's explicit answer to the SAS comparison.
    pub(super) fn pair_answer(&mut self, request: InstallerRequest) {
        if self.job(steps::PAIR, JobStage::Apply).is_none() || !self.connect.polling {
            return;
        }
        let kind = match request {
            InstallerRequest::PairPick { .. } => OwnCall::PairPick,
            _ => OwnCall::PairConfirm,
        };
        if let Err(failure) = self.call(kind, request) {
            self.details
                .insert(steps::PAIR, failure_text(&failure).into());
        }
    }

    pub(super) fn pair_answered(&mut self, reply: AgentReply) {
        if let Err(failure) = reply.result {
            self.connect.polling = false;
            self.details
                .insert(steps::PAIR, failure_text(&failure).into());
            self.apply_outcome(steps::PAIR, outcome(&failure));
        }
    }

    /// The reply to an agent-applied step's request. The acknowledgement only says the agent took
    /// the request; the step is verified later from a Status.
    pub(super) fn step_apply_reply(&mut self, reply: AgentReply) {
        // Correlated by call id to the exact Apply job it was sent for; a reply for a retired
        // job is dropped.
        let Some(job) = self.step_apply_calls.remove(&reply.id) else {
            return;
        };
        if self.jobs.get(&job.step) != Some(&job) {
            return;
        }
        let applied = match reply.result {
            Ok(_) => ApplyOutcome::Applied,
            Err(failure) => {
                self.details
                    .insert(job.step, bounded(failure_text(&failure)));
                outcome(&failure)
            }
        };
        let _ = self.reduce(FlowEvent::Applied {
            step: job.step,
            operation: job.operation,
            outcome: applied,
        });
    }

    pub(super) fn scan(&mut self) {
        if self.summary.milestone != crosspane_installer_core::Milestone::NotInstalled
            && !self.own_calls.values().any(|k| *k == OwnCall::PairScan)
        {
            let _ = self.call(OwnCall::PairScan, InstallerRequest::PairScan);
        }
    }

    pub(super) fn scan_reply(&mut self, reply: AgentReply) {
        if let Ok(DecodedReply::PairScan(list)) = reply.result {
            self.connect.candidates = list.into_iter().take(MAX_CANDIDATES).collect();
        }
    }

    pub(super) fn connect_tick(&mut self) {
        self.settings_tick();
        if self.connect.polling {
            if self.now.saturating_sub(self.connect.started_at) > PAIR_TIMEOUT_MS {
                self.connect.polling = false;
                self.details
                    .insert(steps::PAIR, "Pairing timed out. Try again.".into());
                self.apply_outcome(steps::PAIR, ApplyOutcome::Failed);
            } else if self.now >= self.connect.poll_at.saturating_add(PAIR_POLL_MS)
                && !self.own_calls.values().any(|k| *k == OwnCall::PairStatus)
            {
                self.connect.poll_at = self.now;
                let _ = self.call(OwnCall::PairStatus, InstallerRequest::PairStatus);
            }
        }
        for step in [steps::PAIR, steps::GRANTS, steps::LAYOUT] {
            if let (Some(job), Some((_, started))) = (
                self.job(step, JobStage::Verify),
                self.connect.verify_from.get(&step).copied(),
            ) && self.now.saturating_sub(started) > VERIFY_TIMEOUT_MS
            {
                self.details.insert(
                    step,
                    "The agent hasn't confirmed this yet. Check again.".into(),
                );
                if step == steps::LAYOUT {
                    self.layout_settled(false);
                }
                let _ = self.reduce(FlowEvent::Waiting {
                    step,
                    operation: job.operation,
                    kind: WaitKind::Peer,
                });
            }
        }
    }

    pub(super) fn dispatch_grants(&mut self, job: JobIntent) {
        match job.stage {
            JobStage::Detect => self.resolve_detect(&job),
            JobStage::Plan => {
                self.connect.grants = self.granted();
                self.planned(
                    &job,
                    "Choose what the other computer may do here. Practice needs all five; you \
                     can change them later in Crosspane's settings.",
                );
            }
            JobStage::Apply => {
                let Some(peer) = self.peer else {
                    self.apply_outcome(steps::GRANTS, ApplyOutcome::Failed);
                    return;
                };
                let current = self.granted();
                self.connect.grants_pending.clear();
                self.connect.grants_outcome = None;
                for (i, c) in CAPABILITIES.iter().enumerate() {
                    if current[i] == self.connect.grants[i] {
                        continue;
                    }
                    match self.call(
                        OwnCall::Allow,
                        InstallerRequest::Allow {
                            peer,
                            capability: *c,
                            allow: self.connect.grants[i],
                        },
                    ) {
                        Ok(id) => {
                            self.connect.grants_pending.insert(id);
                        }
                        Err(failure) => {
                            self.connect.grants_outcome = Some(outcome(&failure));
                        }
                    }
                }
                self.finish_grants();
            }
            JobStage::Verify => self.start_verify(job.step),
        }
    }

    fn finish_grants(&mut self) {
        if self.connect.grants_pending.is_empty() {
            let result = self
                .connect
                .grants_outcome
                .take()
                .unwrap_or(ApplyOutcome::Applied);
            self.apply_outcome(steps::GRANTS, result);
        }
    }

    pub(super) fn allow_reply(&mut self, reply: AgentReply) {
        if !self.connect.grants_pending.remove(&reply.id) {
            return;
        }
        if let Err(failure) = &reply.result {
            self.details
                .insert(steps::GRANTS, failure_text(failure).into());
            let worse = outcome(failure);
            self.connect.grants_outcome = Some(match (self.connect.grants_outcome, worse) {
                (Some(ApplyOutcome::Unknown), _) | (_, ApplyOutcome::Unknown) => {
                    ApplyOutcome::Unknown
                }
                (Some(ApplyOutcome::Failed), _) => ApplyOutcome::Failed,
                (_, w) => w,
            });
        }
        self.finish_grants();
    }

    pub(super) fn grant_toggle(&mut self, index: usize, checked: bool) {
        if self.step_state(steps::GRANTS) == StepState::NeedsAction && index < 5 {
            self.connect.grants[index] = checked;
        }
    }

    pub(super) fn dispatch_layout(&mut self, job: JobIntent) {
        match job.stage {
            JobStage::Detect => {
                let _ = self.reduce(FlowEvent::Detected {
                    step: job.step,
                    operation: job.operation,
                    needs_action: true,
                });
            }
            JobStage::Plan => {
                self.planned(
                    &job,
                    "Drag the screens to match your desk and apply, or keep the layout shown.",
                );
                if self.connect.place.is_some() {
                    self.request_apply(steps::LAYOUT);
                }
            }
            JobStage::Apply => match self.connect.place.take() {
                Some(placements) => {
                    self.connect.placed = Some(placements.clone());
                    self.connect.layout_busy = true;
                    if let Err(failure) =
                        self.call(OwnCall::Place, InstallerRequest::Place { placements })
                    {
                        self.details
                            .insert(steps::LAYOUT, failure_text(&failure).into());
                        self.layout_settled(false);
                        self.apply_outcome(steps::LAYOUT, outcome(&failure));
                    }
                }
                None => {
                    self.connect.placed = None;
                    self.apply_outcome(steps::LAYOUT, ApplyOutcome::Applied);
                }
            },
            JobStage::Verify => self.start_verify(job.step),
        }
    }

    pub(super) fn place_reply(&mut self, reply: AgentReply) {
        if self.job(steps::LAYOUT, JobStage::Apply).is_none() {
            return;
        }
        match reply.result {
            Ok(_) => self.apply_outcome(steps::LAYOUT, ApplyOutcome::Applied),
            Err(failure) => {
                self.details
                    .insert(steps::LAYOUT, failure_text(&failure).into());
                self.layout_settled(false);
                self.apply_outcome(steps::LAYOUT, outcome(&failure));
            }
        }
    }

    fn layout_settled(&mut self, committed: bool) {
        self.connect.layout_busy = false;
        self.effects.push(if committed {
            ShellEffect::FollowLayout
        } else {
            ShellEffect::RevertLayout
        });
    }

    pub(super) fn layout_action(&mut self, action: LayoutAction) {
        let LayoutAction::Apply(intents) = action else {
            return;
        };
        if self.connect.layout_busy {
            return;
        }
        let Some(h) = self.health_ref() else {
            return;
        };
        let mut nodes: BTreeMap<String, NodeId> = BTreeMap::new();
        nodes.insert(h.installer().node.to_string(), h.installer().node);
        for p in &h.installer().peers {
            nodes.insert(p.node.to_string(), p.node);
        }
        let mut placements = Vec::new();
        for intent in intents {
            let Some(node) = nodes.get(&intent.node) else {
                self.notice = Some("That layout names a screen this computer doesn't know.".into());
                self.effects.push(ShellEffect::RevertLayout);
                return;
            };
            placements.push(Placement {
                node: *node,
                display: intent.display,
                origin_mm: intent.origin_mm,
            });
        }
        if placements.is_empty() {
            return;
        }
        self.connect.place = Some(placements);
        match self.step_state(steps::LAYOUT) {
            StepState::NeedsAction => self.request_apply(steps::LAYOUT),
            StepState::Checking | StepState::Planning => {}
            _ => self.begin(steps::LAYOUT),
        }
    }

    pub(super) fn accept_layout(&mut self) {
        self.connect.place = None;
        self.request_apply(steps::LAYOUT);
    }

    pub(super) fn dispatch_final(&mut self, job: JobIntent) {
        match job.stage {
            JobStage::Detect => {
                let _ = self.reduce(FlowEvent::Detected {
                    step: job.step,
                    operation: job.operation,
                    needs_action: false,
                });
            }
            JobStage::Verify => self.verify_final(&job),
            JobStage::Plan | JobStage::Apply => {}
        }
    }

    /// Fresh, bound health from exactly the ledger's admitted local sample.
    fn verify_final(&mut self, job: &JobIntent) {
        let Some(health) = self.health.as_ref() else {
            return;
        };
        let Some(sample) = self.ledger.local().cloned() else {
            return;
        };
        if sample.observed_at_ms != health.observed_at_ms {
            return;
        }
        if let Err(reason) = final_healthy(&health.snapshot, self.peer) {
            self.details.insert(steps::FINAL, reason.into());
            let _ = self.reduce(FlowEvent::Failed {
                step: job.step,
                operation: job.operation,
            });
            return;
        }
        self.details.remove(&steps::FINAL);
        let _ = self.reduce(FlowEvent::Verified {
            step: job.step,
            operation: job.operation,
            verification: Verification {
                source: sample.source,
                observed_at_ms: sample.observed_at_ms,
                binding: Some(sample.binding.clone()),
                activity: None,
                human_attempt: None,
                fixture_attempt: None,
            },
        });
    }

    /// Re-verify final health on every own Status once all practice holds, so readiness lapses
    /// honestly and is renewed without waiting for the five-second expiry.
    fn reverify_final(&mut self) {
        if !self
            .graph
            .practice_steps()
            .iter()
            .all(|s| self.satisfied(*s))
        {
            return;
        }
        if let Some(job) = self.job(steps::FINAL, JobStage::Verify) {
            self.verify_final(&job);
            return;
        }
        let _ = self.reduce(FlowEvent::Begin { step: steps::FINAL });
        self.dispatch_intents();
    }

    pub(super) fn after_status(&mut self, call: u64) {
        let Some((at, source)) = self.health.as_ref().map(|h| (h.observed_at_ms, h.source)) else {
            return;
        };
        if self.satisfied(steps::PAIR) && !self.peer.is_some_and(|p| self.connected(p)) {
            // A paired peer that disappears from status (unpaired or revoked) retires pairing.
            let known = self.peer.is_some_and(|p| {
                self.health_ref()
                    .is_some_and(|h| h.installer().peers.iter().any(|x| x.node == p))
            });
            if !known {
                self.peer = None;
                let _ = self.reduce(FlowEvent::Invalidate { step: steps::PAIR });
            }
        }
        if self.satisfied(steps::GRANTS) && self.granted() != [true; 5] {
            let _ = self.reduce(FlowEvent::Invalidate {
                step: steps::GRANTS,
            });
        }
        if self.satisfied(steps::LAYOUT) && !self.layout_committed() {
            let _ = self.reduce(FlowEvent::Invalidate {
                step: steps::LAYOUT,
            });
        }
        for step in [steps::PAIR, steps::GRANTS] {
            if let Some(job) = self.job(step, JobStage::Detect) {
                self.resolve_detect(&job);
            }
        }
        // Something done elsewhere (the other computer, the settings app) can meet a goal that
        // is waiting for the person here; re-detect instead of asking for it again.
        if self.step_state(steps::PAIR) == StepState::NeedsAction
            && self.connect.mode.is_none()
            && self.current_peer().is_some()
        {
            self.begin(steps::PAIR);
        }
        if self.step_state(steps::GRANTS) == StepState::NeedsAction
            && self.peer.is_some()
            && self.granted() == [true; 5]
        {
            self.begin(steps::GRANTS);
        }
        self.dispatch_intents();
        for step in [steps::PAIR, steps::GRANTS, steps::LAYOUT] {
            let Some(job) = self.job(step, JobStage::Verify) else {
                continue;
            };
            if self
                .connect
                .verify_from
                .get(&step)
                .is_none_or(|(from, _)| call < *from)
            {
                continue;
            }
            match step {
                steps::PAIR => {
                    let fresh: Vec<NodeId> = self
                        .connected_peers()
                        .into_iter()
                        .map(|(n, _)| n)
                        .filter(|n| !self.connect.known.contains(n))
                        .collect();
                    let chosen = match self.connect.mode.clone() {
                        Some(PairMode::Existing(n)) => self.connected(n).then_some(n),
                        // A pairing the person just made selects only the computer it paired:
                        // a newly connected one, or (re-pairing) the one with the paired name.
                        // Another, older peer that happens to be connected is never chosen.
                        Some(PairMode::Listen | PairMode::Join(_)) => self.newly_paired(&fresh),
                        _ if fresh.len() == 1 => Some(fresh[0]),
                        _ => self.current_peer(),
                    };
                    if let Some(peer) = chosen {
                        self.peer = Some(peer);
                        self.connect.mode = None;
                        self.verified(&job, source, at);
                    }
                }
                steps::GRANTS => {
                    if self.granted() == [true; 5] {
                        self.verified(&job, source, at);
                    } else {
                        self.details.insert(
                            step,
                            "Practice needs all five permissions. Turn them on to continue.".into(),
                        );
                        let _ = self.reduce(FlowEvent::Waiting {
                            step,
                            operation: job.operation,
                            kind: WaitKind::User,
                        });
                    }
                }
                _ => {
                    if self.layout_committed() && self.placed_matches() {
                        self.layout_settled(true);
                        self.verified(&job, source, at);
                    } else {
                        self.details.insert(
                            step,
                            "The agent didn't commit this layout. Arrange the screens again."
                                .into(),
                        );
                        self.layout_settled(false);
                        let _ = self.reduce(FlowEvent::Waiting {
                            step,
                            operation: job.operation,
                            kind: WaitKind::User,
                        });
                    }
                }
            }
        }
        self.recheck_traffic_steps();
        if self.own_calls_were(call) {
            self.reverify_final();
        }
        self.dispatch_intents();
    }

    /// The computer a Listen or Join pairing just paired, once it is connected.
    fn newly_paired(&self, fresh: &[NodeId]) -> Option<NodeId> {
        if let [only] = fresh {
            return Some(*only);
        }
        if !fresh.is_empty() {
            return None;
        }
        let name = self.connect.pairing.as_ref()?.peer.as_ref()?;
        let named: Vec<NodeId> = self
            .health_ref()?
            .installer()
            .peers
            .iter()
            .filter(|p| p.connected && &p.name == name)
            .map(|p| p.node)
            .collect();
        match named.as_slice() {
            [only] => Some(*only),
            _ => None,
        }
    }

    fn own_calls_were(&self, call: u64) -> bool {
        self.health.as_ref().is_some_and(|h| h.call == call)
            && self.ledger.local().map(|s| s.observed_at_ms)
                == self.health.as_ref().map(|h| h.observed_at_ms)
    }

    pub(super) fn edit_address(&mut self, field: u16, value: String) {
        if field == super::ids::PEER_ADDRESS {
            let mut value: String = value.chars().filter(|c| !c.is_control()).collect();
            value.truncate(
                (0..=MAX_ADDRESS.min(value.len()))
                    .rev()
                    .find(|i| value.is_char_boundary(*i))
                    .unwrap_or(0),
            );
            self.connect.address = value;
        }
    }

    pub(super) fn choose_hiding(&mut self, choice: HidingChoice) {
        if self.desc.hiding_choice {
            self.connect.hiding = Some(choice);
        }
    }

    /// The macOS hiding choice through the frozen settings transition: an explicit update consent,
    /// then an explicit restart consent, verified only by the new instance's loaded setting.
    pub(super) fn dispatch_hiding(&mut self, job: JobIntent) {
        match job.stage {
            JobStage::Detect => {
                let _ = self.reduce(FlowEvent::Detected {
                    step: job.step,
                    operation: job.operation,
                    needs_action: true,
                });
            }
            JobStage::Plan => {
                self.connect.settings = None;
                self.planned(
                    &job,
                    "Choose what happens on this Mac while one of its windows is shown on the \
                     other computer. Hide moves it onto a private virtual display; Mirror keeps \
                     it visible here. Applying restarts Crosspane.",
                );
            }
            JobStage::Apply => self.start_settings(),
            JobStage::Verify => {
                let complete = self.connect.settings.as_ref().is_some_and(|t| {
                    *t.state() == crate::tutorial_flow::SettingsTransitionState::Complete
                });
                if let (true, Some((at, source))) = (
                    complete,
                    self.health.as_ref().map(|h| (h.observed_at_ms, h.source)),
                ) {
                    self.verified(&job, source, at);
                }
            }
        }
    }

    fn start_settings(&mut self) {
        let Some((snapshot, source, at)) = self
            .health
            .as_ref()
            .map(|h| (h.snapshot.clone(), h.source, h.observed_at_ms))
        else {
            self.apply_outcome(steps::HIDING, ApplyOutcome::Failed);
            return;
        };
        let hide = self.connect.hiding == Some(HidingChoice::Hide);
        let revision = self.view.revision;
        let mut transition =
            crate::tutorial_flow::SettingsTransition::new(snapshot.installer().node, revision);
        if let Some(peer) = self.peer {
            let _ = transition.track_peers(&[peer]);
        }
        match transition.detected(&snapshot, source, at, self.now, revision) {
            Ok(events) => {
                for event in events {
                    let _ = self.reduce(event);
                }
            }
            Err(_) => {
                self.details.insert(
                    steps::HIDING,
                    "This computer's agent status can't be used for this change.".into(),
                );
                self.apply_outcome(steps::HIDING, ApplyOutcome::Failed);
                return;
            }
        }
        let id = self.alloc_call_id();
        let call = transition.consent_update(id, revision, hide);
        self.connect.settings = Some(transition);
        match call {
            Ok(call) => {
                if let Err(failure) = self.submit_call(OwnCall::Settings, call) {
                    self.details
                        .insert(steps::HIDING, failure_text(&failure).into());
                    self.apply_outcome(steps::HIDING, outcome(&failure));
                }
            }
            Err(_) => self.apply_outcome(steps::HIDING, ApplyOutcome::Failed),
        }
    }

    pub(super) fn hiding_restart(&mut self) {
        use crate::tutorial_flow::SettingsTransitionState as S;
        let mut tracked = self.graph.practice_steps();
        tracked.push(steps::FINAL);
        let id = self.alloc_call_id();
        let Some(transition) = self.connect.settings.as_mut() else {
            return;
        };
        if !matches!(
            transition.state(),
            S::NeedsRestartConsent | S::NeedsRecoveryRestartConsent
        ) {
            return;
        }
        let samples = transition.current_samples().to_vec();
        let view = transition.view_revision();
        match transition.consent_restart(id, view, &tracked, &samples) {
            Ok((events, call)) => {
                for event in events {
                    let _ = self.reduce(event);
                }
                if let Err(failure) = self.submit_call(OwnCall::Settings, call) {
                    self.details
                        .insert(steps::HIDING, failure_text(&failure).into());
                }
            }
            Err(_) => {
                self.notice = Some("That restart is no longer valid. Choose again.".into());
            }
        }
    }

    pub(super) fn settings_reply(&mut self, reply: AgentReply) {
        use crate::tutorial_flow::SettingsTransitionState as S;
        let Some(transition) = self.connect.settings.as_mut() else {
            return;
        };
        let Ok(outcome_) = transition.reply(reply, self.now) else {
            return;
        };
        let state = transition.state().clone();
        for event in outcome_.observations {
            let _ = self.reduce(event);
        }
        match state {
            S::Complete => self.apply_outcome(steps::HIDING, ApplyOutcome::Applied),
            S::Failed(failure) => {
                self.details
                    .insert(steps::HIDING, failure_text(&failure).into());
                self.apply_outcome(steps::HIDING, ApplyOutcome::Failed);
            }
            S::NeedsDetection if outcome_.detect_after_unknown => {
                self.apply_outcome(steps::HIDING, ApplyOutcome::Unknown);
            }
            _ => {}
        }
        self.dispatch_intents();
    }

    pub(super) fn settings_tick(&mut self) {
        use crate::tutorial_flow::SettingsTransitionState as S;
        let waiting = self
            .connect
            .settings
            .as_ref()
            .is_some_and(|t| *t.state() == S::WaitingNewInstance);
        if !waiting
            || self.own_calls.values().any(|k| *k == OwnCall::Settings)
            || self.now < self.connect.poll_at.saturating_add(PAIR_POLL_MS)
        {
            return;
        }
        self.connect.poll_at = self.now;
        let id = self.alloc_call_id();
        let call = self
            .connect
            .settings
            .as_mut()
            .and_then(|t| t.poll_new_instance(id).ok());
        if let Some(call) = call {
            let _ = self.submit_call(OwnCall::Settings, call);
        }
    }

    pub(super) fn hiding_restart_pending(&self) -> bool {
        use crate::tutorial_flow::SettingsTransitionState as S;
        self.connect.settings.as_ref().is_some_and(|t| {
            matches!(
                t.state(),
                S::NeedsRestartConsent | S::NeedsRecoveryRestartConsent
            )
        })
    }
}
