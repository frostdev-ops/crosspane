//! Signed-agent permission guidance. No permission preflight or native dispatch lives here.
//! WP-4.22 dispatches the enum-only OpenPane intent and correlates its completion; it must
//! reduce Core observations and retire readiness proofs before dispatching a restart.
use super::{native_io::*, transport::SelectedAgent};
use crate::{
    agent_contract::*,
    settings_transition::{SettingsTransition, SettingsTransitionState},
    view::{HidingChoice, ScreenId},
};
use crosspane_installer_core::FlowEvent;
use crosspane_types::id::NodeId;
use std::path::{Path, PathBuf};

pub const MICROPHONE_REASON: &str =
    "lets Crosspane hear the Crosspane speakers device; your real microphone is never opened";
pub const MICROPHONE_DETAIL: &str = "Authorization is for the hidden Crosspane speakers input. Sharing the mic peer capability remains unavailable.";
pub const ASK_EXPLANATION: &str = "Crosspane asks for one permission at a time. macOS shows its request, or opens System Settings at the right place when that request was already answered. A request that was shown does not mean permission was granted.";
pub const NETWORK_EXPLANATION: &str = "Allow the signed Crosspane agent to find and connect to your other computer on the Local Network. A system prompt may appear. An empty search does not establish a privacy denial.";
pub const HIDE_LABEL: &str = "Hide projected windows on a virtual display (recommended)";
pub const MIRROR_LABEL: &str = "Mirror instead (windows stay visible on this Mac)";
pub const HIDE_EXPLANATION: &str = "Uses an Apple private interface approved by Crosspane's owner. Crosspane falls back to mirroring if that interface stops working.";

/// The complete allowlist, including the fourth producer pane. There is no raw-URL case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsPane {
    ScreenRecording,
    Accessibility,
    InputMonitoring,
    Microphone,
}
impl SettingsPane {
    pub fn url(self) -> &'static str {
        match self {
            Self::ScreenRecording => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            Self::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            Self::InputMonitoring => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
            }
            Self::Microphone => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            }
        }
    }
    pub fn breadcrumbs(self) -> &'static str {
        match self {
            Self::ScreenRecording => {
                "System Settings > Privacy & Security > Screen & System Audio Recording > Crosspane"
            }
            // macOS 27 renamed the Accessibility pane (WP-4.33).
            Self::Accessibility if running_macos_major().is_some_and(|major| major >= 27) => {
                "System Settings > Privacy & Security > Device Control and Data Access > Crosspane"
            }
            Self::Accessibility => {
                "System Settings > Privacy & Security > Accessibility > Crosspane"
            }
            Self::InputMonitoring => {
                "System Settings > Privacy & Security > Input Monitoring > Crosspane"
            }
            Self::Microphone => "System Settings > Privacy & Security > Microphone > Crosspane",
        }
    }
    pub fn permission(self) -> PermissionName {
        match self {
            Self::ScreenRecording => PermissionName::ScreenRecording,
            Self::Accessibility => PermissionName::Accessibility,
            Self::InputMonitoring => PermissionName::InputMonitoring,
            Self::Microphone => PermissionName::Microphone,
        }
    }
}
/// This Mac's macOS major version, read once (pane names change between versions).
pub fn running_macos_major() -> Option<u64> {
    static MAJOR: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *MAJOR.get_or_init(|| {
        let version = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
        u64::try_from(version.majorVersion).ok()
    })
}
#[derive(Clone, PartialEq, Eq)]
struct Principal {
    home: PathBuf,
    runtime: PathBuf,
    uid: u32,
    requirement: SigningRequirement,
    team: String,
}
/// Opaque admission from the frozen native seam, produced on a bounded detector worker.
pub struct GuideAdmission {
    principal: Principal,
    instance: InstanceStatus,
    phase: BootstrapPhase,
    phase_seq: u64,
    source: ObservationSource,
    at: u64,
}
impl std::fmt::Debug for GuideAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GuideAdmission")
    }
}
impl GuideAdmission {
    #[cfg(test)]
    #[allow(dead_code)] // Used by the integration suite's privately compiled guide module.
    pub(crate) fn emulate_live(&mut self) {
        self.source = ObservationSource::Live;
    }
    fn observed(
        io: &MacNativeIo,
        support: &SupportProof,
        main: &SignatureProof,
        deadline: &Deadline,
    ) -> NativeResult<Self> {
        support.check(io, deadline)?;
        if main.path() != io.target().agent_path() || main.requirement().role != ArtifactRole::Agent
        {
            return Err(NativeError::Foreign);
        }
        let (bootstrap, process) = io.bootstrap(main, deadline)?;
        // admit_support pins both the configured signing requirement and GUI target.
        io.admit_support(main, deadline)?;
        Ok(Self {
            principal: Principal {
                home: io.target().paths().home.clone(),
                runtime: io.target().runtime_dir().to_owned(),
                uid: process.uid,
                requirement: main.requirement().clone(),
                team: main.observation().team_identifier.clone(),
            },
            instance: InstanceStatus {
                id: bootstrap.instance_id,
                pid: process.pid,
                uid: Some(process.uid),
                exe: process.executable.to_string_lossy().into_owned(),
                runtime_dir: io.target().runtime_dir().to_string_lossy().into_owned(),
                started_unix_ms: bootstrap.started_unix_ms,
            },
            phase: bootstrap.phase,
            phase_seq: bootstrap.phase_seq,
            source: io.target().source(),
            at: io.clock().now_ms(),
        })
    }
    pub fn ready(
        selected: &SelectedAgent,
        main: &SignatureProof,
        deadline: &Deadline,
    ) -> NativeResult<Self> {
        selected
            .instance
            .revalidate(&selected.io, &selected.support, deadline)?;
        let admission = Self::observed(&selected.io, &selected.support, main, deadline)?;
        selected.instance.admit_status(&admission.instance)?;
        if admission.phase != BootstrapPhase::Ready {
            return Err(NativeError::Unavailable);
        }
        Ok(admission)
    }
    /// A verified pre-ready bootstrap never authorizes ctl requests or creates a fallback key.
    pub fn waiting(
        io: &MacNativeIo,
        support: &SupportProof,
        main: &SignatureProof,
        deadline: &Deadline,
    ) -> NativeResult<Self> {
        let admission = Self::observed(io, support, main, deadline)?;
        if admission.phase == BootstrapPhase::Ready {
            return Err(NativeError::Invalid);
        }
        Ok(admission)
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct GuideToken {
    principal: Principal,
    revision: u64,
    generation: u64,
    instance: u64,
}
impl std::fmt::Debug for GuideToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GuideToken")
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuideBinding {
    token: GuideToken,
    operation: u64,
}
#[derive(Clone)]
pub enum GuideIntent {
    Agent {
        binding: GuideBinding,
        call: AgentCall,
    },
    OpenPane {
        binding: GuideBinding,
        pane: SettingsPane,
    },
    Redetect {
        binding: GuideBinding,
    },
    RetireActivity {
        generation: u64,
    },
    Core(Vec<FlowEvent>),
}
impl std::fmt::Debug for GuideIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent { .. } => f.write_str("Agent"),
            Self::OpenPane { pane, .. } => pane.fmt(f),
            Self::Redetect { .. } => f.write_str("Redetect"),
            Self::RetireActivity { .. } => f.write_str("RetireActivity"),
            Self::Core(_) => f.write_str("Core"),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuideReason {
    HealthPending,
    KeychainWaiting,
    StartupWaiting,
    RecoveryPending,
    MigrationPending,
    RequestStarted,
    Unavailable,
    Refused,
    OutcomeUnknown,
    SettingsBreadcrumbs,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkState {
    Explanation,
    UnknownConnectivity,
    WaitingForPeer,
    Connected,
    ReportedDenied,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkingObservation {
    Hidden,
    Visible,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkingVerification {
    Pending,
    Twin,
    Mirror,
    MirrorFallback,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantRow {
    pub permission: PermissionName,
    pub state: PermissionState,
    pub required: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuideView {
    pub token: GuideToken,
    pub screen: ScreenId,
    pub rows: Vec<GrantRow>,
    pub audio_prerequisite: bool,
    pub reason: Option<GuideReason>,
    pub breadcrumbs: Option<&'static str>,
    pub restart_warning: &'static str,
    pub network: NetworkState,
    pub hiding_choice: Option<HidingChoice>,
    pub hiding_next_enabled: bool,
    pub parking: ParkingVerification,
    pub simulated: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuideError {
    Stale,
    Busy,
    Exhausted,
    Invalid,
    NotReady,
}
#[derive(Clone)]
struct Pending {
    binding: GuideBinding,
    call: AgentCall,
    sent: u64,
    settings: bool,
}
struct ProjectionAttempt {
    token: GuideToken,
    peer: NodeId,
    link: u64,
    counter: u64,
    epochs: WireEpochs,
    at: u64,
}
/// One outstanding agent call and one pane completion; all other work is represented as intents.
/// Native admission, byte receipts and every now argument use the same monotonic clock.
pub struct PermissionGuide {
    admission: GuideAdmission,
    local: NodeId,
    revision: u64,
    generation: u64,
    next_id: u64,
    health: Option<Box<HealthSnapshot>>,
    at: u64,
    last_now: u64,
    pending: Option<Pending>,
    pane: Option<(GuideBinding, SettingsPane, u64)>,
    waiting_instance: Option<u64>,
    restart_uncertain: bool,
    needs_detection: bool,
    retired: Vec<u64>,
    counters: Vec<(NodeId, u64)>,
    counter_invalid: bool,
    reason: Option<GuideReason>,
    breadcrumbs: Option<&'static str>,
    network_explained: bool,
    denied: bool,
    selected: SettingsPane,
    choice: Option<HidingChoice>,
    settings: SettingsTransition,
    settings_active: bool,
    attempt: Option<ProjectionAttempt>,
    parking: ParkingVerification,
}
impl std::fmt::Debug for PermissionGuide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PermissionGuide")
    }
}
fn fresh(at: u64, now: u64) -> bool {
    now.checked_sub(at).is_some_and(|age| age <= MAX_TIMEOUT_MS)
}
fn same_instance(observed: &InstanceStatus, expected: &InstanceStatus) -> bool {
    let mut normalized = observed.clone();
    let (Ok(exe), Ok(runtime)) = (
        admitted_spelling(Path::new(&observed.exe)),
        admitted_spelling(Path::new(&observed.runtime_dir)),
    ) else {
        return false;
    };
    normalized.exe = exe.to_string_lossy().into_owned();
    normalized.runtime_dir = runtime.to_string_lossy().into_owned();
    normalized == *expected
}
fn active(h: &HealthSnapshot) -> bool {
    let t = h.terminal();
    t.controlling.is_some()
        || t.controlled_by.is_some()
        || !t.projections.is_empty()
        || !h.installer().audio.active_peers.is_empty()
}
fn phase_reason(phase: BootstrapPhase) -> GuideReason {
    match phase {
        BootstrapPhase::WaitingForKeystore => GuideReason::KeychainWaiting,
        BootstrapPhase::Ready => GuideReason::HealthPending,
        _ => GuideReason::StartupWaiting,
    }
}
fn recovered(h: &HealthSnapshot) -> bool {
    let i = h.installer();
    matches!(
        i.startup_recovery,
        StartupRecovery::Restored | StartupRecovery::NothingParked
    ) && i.recovery_pending == 0
        && i.keystore == KeyStoreProvenance::OsStore
}
impl PermissionGuide {
    pub fn new(
        admission: GuideAdmission,
        local: NodeId,
        now: u64,
        first_call_id: u64,
    ) -> Result<Self, GuideError> {
        if !fresh(admission.at, now) || first_call_id == 0 || first_call_id == u64::MAX {
            return Err(GuideError::Stale);
        }
        let reason = Some(phase_reason(admission.phase));
        Ok(Self {
            at: admission.at,
            admission,
            local,
            revision: 1,
            generation: 1,
            next_id: first_call_id,
            health: None,
            last_now: now,
            pending: None,
            pane: None,
            waiting_instance: None,
            restart_uncertain: false,
            needs_detection: false,
            retired: Vec::new(),
            counters: Vec::new(),
            counter_invalid: false,
            reason,
            breadcrumbs: None,
            network_explained: false,
            denied: false,
            selected: SettingsPane::ScreenRecording,
            choice: None,
            settings: SettingsTransition::new(local, 1),
            settings_active: false,
            attempt: None,
            parking: ParkingVerification::Pending,
        })
    }
    /// WP-4.22 seeds its shared allocator and resumes it after this exclusive guide phase.
    pub fn last_operation_id(&self) -> u64 {
        self.next_id - 1
    }
    pub fn settings_state(&self) -> &SettingsTransitionState {
        self.settings.state()
    }
    fn time(&mut self, now: u64) -> Result<(), GuideError> {
        if now < self.last_now {
            return Err(GuideError::Stale);
        }
        self.last_now = now;
        if !fresh(self.at, now) {
            self.clear_parking();
        }
        Ok(())
    }
    fn clear_parking(&mut self) {
        self.attempt = None;
        self.parking = ParkingVerification::Pending;
    }
    fn clear_health(&mut self) {
        self.health = None;
        self.clear_parking();
    }
    fn retire_activity(&mut self) -> Result<GuideIntent, GuideError> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(GuideError::Exhausted)?;
        self.clear_health();
        Ok(GuideIntent::RetireActivity {
            generation: self.generation,
        })
    }
    fn bump(&mut self) -> Result<(), GuideError> {
        self.revision = self.revision.checked_add(1).ok_or(GuideError::Exhausted)?;
        Ok(())
    }
    fn token(&self) -> GuideToken {
        GuideToken {
            principal: self.admission.principal.clone(),
            revision: self.revision,
            generation: self.generation,
            instance: self.admission.instance.id,
        }
    }
    fn current(&mut self, token: &GuideToken, now: u64) -> Result<(), GuideError> {
        self.time(now)?;
        if token != &self.token() || !fresh(self.at, now) {
            return Err(GuideError::Stale);
        }
        if self.reason == Some(GuideReason::MigrationPending) {
            return Err(GuideError::NotReady);
        }
        Ok(())
    }
    fn binding(&mut self) -> Result<GuideBinding, GuideError> {
        let operation = self.next_id;
        self.next_id = operation
            .checked_add(1)
            .filter(|id| *id < u64::MAX)
            .ok_or(GuideError::Exhausted)?;
        Ok(GuideBinding {
            token: self.token(),
            operation,
        })
    }
    fn ready(&self) -> Result<&HealthSnapshot, GuideError> {
        if self.waiting_instance.is_some() || self.admission.phase != BootstrapPhase::Ready {
            return Err(GuideError::NotReady);
        }
        let health = self.health.as_deref().ok_or(GuideError::NotReady)?;
        if !recovered(health) {
            return Err(GuideError::NotReady);
        }
        Ok(health)
    }
    fn enqueue(
        &mut self,
        request: InstallerRequest,
        now: u64,
        settings: bool,
    ) -> Result<Vec<GuideIntent>, GuideError> {
        if self.pending.is_some() {
            return Err(GuideError::Busy);
        }
        let binding = self.binding()?;
        let call = AgentCall {
            id: binding.operation,
            request,
            timeout_ms: MAX_TIMEOUT_MS,
        };
        self.pending = Some(Pending {
            binding: binding.clone(),
            call: call.clone(),
            sent: now,
            settings,
        });
        self.bump()?;
        Ok(vec![GuideIntent::Agent { binding, call }])
    }
    /// Polling is the only timer-driven call. It never asks, restarts, dials, or opens Settings.
    /// Dispatch on the same selected port as the binding; QueueFull means no I/O was started.
    pub fn poll_health(&mut self, now: u64) -> Result<Vec<GuideIntent>, GuideError> {
        self.time(now)?;
        if self.admission.phase != BootstrapPhase::Ready
            || (self.waiting_instance.is_some() && !self.restart_uncertain)
            || self.needs_detection
            || self.reason == Some(GuideReason::MigrationPending)
        {
            return Ok(vec![GuideIntent::Redetect {
                binding: self.binding()?,
            }]);
        }
        if self.pending.is_some() {
            return Ok(Vec::new());
        }
        let settings = self.settings_active
            && self.settings.state() == &SettingsTransitionState::WaitingNewInstance;
        if settings {
            let id = self.next_id;
            let call = self
                .settings
                .poll_new_instance(id)
                .map_err(|_| GuideError::Invalid)?;
            return self.enqueue(call.request, now, true);
        }
        self.enqueue(InstallerRequest::Status, now, false)
    }
    /// Every redetection is a fresh native admission; never bind a replacement to an old port.
    pub fn redetected(
        &mut self,
        admission: GuideAdmission,
        now: u64,
    ) -> Result<Vec<GuideIntent>, GuideError> {
        self.time(now)?;
        if !fresh(admission.at, now) || admission.at < self.at {
            return Err(GuideError::Stale);
        }
        if self.reason == Some(GuideReason::MigrationPending) {
            return Err(GuideError::NotReady);
        }
        if admission.principal != self.admission.principal {
            let retirement = self.retire_activity()?;
            self.pending = None;
            self.pane = None;
            self.settings = SettingsTransition::new(self.local, self.revision);
            self.settings_active = false;
            self.choice = None;
            self.at = now;
            self.reason = Some(GuideReason::MigrationPending);
            self.bump()?;
            return Ok(vec![retirement]);
        }
        if admission.source != self.admission.source {
            return Err(GuideError::Invalid);
        }
        let changed = admission.instance.id != self.admission.instance.id;
        if (!changed
            && (admission.instance != self.admission.instance
                || admission.phase_seq < self.admission.phase_seq))
            || self.retired.contains(&admission.instance.id)
        {
            return Err(GuideError::Stale);
        }
        let mut intents = Vec::new();
        if changed {
            if self.retired.len() == MAX_ITEMS {
                return Err(GuideError::Exhausted);
            }
            self.retired.push(self.admission.instance.id);
            intents.push(self.retire_activity()?);
            self.counters.clear();
            self.counter_invalid = false;
            if let Some(p) = self.pending.as_ref().filter(|p| p.settings) {
                let outcome = self
                    .settings
                    .reply(
                        AgentReply {
                            id: p.call.id,
                            observed_at_ms: admission.at,
                            source: admission.source,
                            result: Err(CallFailure::TimeoutOutcomeUnknown),
                        },
                        now,
                    )
                    .map_err(|_| GuideError::Invalid)?;
                intents.push(GuideIntent::Core(outcome.observations));
            }
            self.pending = None;
            self.pane = None;
            if !self.settings_active
                || self.settings.state() != &SettingsTransitionState::WaitingNewInstance
            {
                self.settings = SettingsTransition::new(self.local, self.revision);
                self.settings_active = false;
                self.choice = None;
            }
            self.network_explained = false;
            self.denied = false;
        }
        if self
            .waiting_instance
            .is_some_and(|old| old != admission.instance.id)
        {
            self.waiting_instance = None;
            self.restart_uncertain = false;
        }
        self.clear_health();
        self.needs_detection = false;
        self.at = admission.at;
        self.admission = admission;
        self.reason = Some(phase_reason(self.admission.phase));
        self.bump()?;
        Ok(intents)
    }
    /// Routing must return the original binding, even after a view changes. Late/duplicate
    /// replies never acquire a replacement operation. Status receipt time is byte-receipt time.
    pub fn reply(
        &mut self,
        binding: &GuideBinding,
        reply: AgentReply,
        now: u64,
    ) -> Result<Vec<GuideIntent>, GuideError> {
        self.time(now)?;
        let pending = self.pending.as_ref().ok_or(GuideError::Stale)?;
        let negative = matches!(
            reply.result,
            Err(_)
                | Ok(DecodedReply::Status(
                    StatusAdmission::PendingHealthContract(_)
                ))
        );
        let unadmitted = reply.source != self.admission.source;
        if &pending.binding != binding
            || reply.id != pending.call.id
            || (unadmitted && (!negative || reply.source != ObservationSource::Demo))
            || (!negative && self.needs_detection)
            || reply.observed_at_ms < pending.sent
            || reply.observed_at_ms < self.at
            || !fresh(reply.observed_at_ms, now)
        {
            return Err(GuideError::Stale);
        }
        if let Ok(DecodedReply::Status(StatusAdmission::Supported(h))) = &reply.result
            && (h.installer().node != self.local
                || !same_instance(&h.installer().instance, &self.admission.instance))
        {
            return Err(GuideError::Invalid);
        }
        let pending = self.pending.take().ok_or(GuideError::Stale)?;
        let mut intents = Vec::new();
        if pending.settings {
            // A local negative outcome settles the frozen transition without promoting a
            // Demo receipt into positive Live evidence, just as the deadline path does.
            let settled = if unadmitted {
                AgentReply {
                    source: self.admission.source,
                    result: Err(CallFailure::TimeoutOutcomeUnknown),
                    ..reply.clone()
                }
            } else {
                reply.clone()
            };
            let outcome = self
                .settings
                .reply(settled, now)
                .map_err(|_| GuideError::Invalid)?;
            intents.push(GuideIntent::Core(outcome.observations));
            if outcome.detect_after_unknown {
                intents.push(GuideIntent::Redetect {
                    binding: self.binding()?,
                });
            }
        }
        match reply.result {
            Ok(DecodedReply::Status(StatusAdmission::Supported(h)))
                if pending.call.request == InstallerRequest::Status =>
            {
                self.reason = if recovered(&h) {
                    None
                } else {
                    Some(GuideReason::RecoveryPending)
                };
                for p in &h.installer().peers {
                    let count = p.counters.e2_source_started;
                    if let Some((_, old)) =
                        self.counters.iter_mut().find(|(node, _)| *node == p.node)
                    {
                        self.counter_invalid |= count < *old;
                        *old = count;
                    } else if self.counters.len() < MAX_ITEMS {
                        self.counters.push((p.node, count));
                    } else {
                        self.counter_invalid = true;
                    }
                }
                if self.counter_invalid
                    || !recovered(&h)
                    || self
                        .health
                        .as_ref()
                        .is_some_and(|old| old.installer().epochs != h.installer().epochs)
                {
                    self.clear_parking();
                }
                if self.parking != ParkingVerification::Pending
                    && !self.attempt.as_ref().is_some_and(|a| {
                        h.installer().peers.iter().any(|p| {
                            p.node == a.peer
                                && p.connected
                                && p.link_generation == Some(a.link)
                                && a.counter.checked_add(1) == Some(p.counters.e2_source_started)
                                && p.last_source_parking
                                    == Some(if self.parking == ParkingVerification::Twin {
                                        SourceParking::Twin
                                    } else {
                                        SourceParking::Mirror
                                    })
                        })
                    })
                {
                    self.clear_parking();
                }
                self.health = Some(h);
                self.at = reply.observed_at_ms;
            }
            Ok(DecodedReply::Status(StatusAdmission::PendingHealthContract(_)))
                if pending.call.request == InstallerRequest::Status =>
            {
                intents.push(self.retire_activity()?);
                self.reason = Some(GuideReason::HealthPending);
            }
            Ok(DecodedReply::Acknowledged)
                if pending.call.request == InstallerRequest::AskPermissions =>
            {
                self.reason = Some(GuideReason::RequestStarted)
            }
            Ok(DecodedReply::Acknowledged)
                if matches!(pending.call.request, InstallerRequest::Dial { .. }) =>
            {
                self.reason = Some(GuideReason::RequestStarted)
            }
            Ok(DecodedReply::Acknowledged) if pending.call.request == InstallerRequest::Restart => {
                self.reason = Some(GuideReason::StartupWaiting);
            }
            Ok(DecodedReply::SettingsUpdated(_)) if pending.settings => {}
            Err(error) => {
                intents.push(self.retire_activity()?);
                self.reason = Some(match error {
                    CallFailure::Refused(_) => GuideReason::Refused,
                    CallFailure::TimeoutOutcomeUnknown => GuideReason::OutcomeUnknown,
                    _ => GuideReason::Unavailable,
                });
                if pending.call.request == InstallerRequest::Restart {
                    self.restart_uncertain = !(matches!(
                        error,
                        CallFailure::QueueFull
                            | CallFailure::InvalidCall(_)
                            | CallFailure::Refused(_)
                    ) || (unadmitted
                        && error == CallFailure::Unavailable));
                    if self.restart_uncertain {
                        self.reason = Some(GuideReason::OutcomeUnknown);
                    } else {
                        self.waiting_instance = None;
                    }
                }
                self.needs_detection = true;
                intents.push(GuideIntent::Redetect {
                    binding: self.binding()?,
                });
            }
            _ => {
                self.clear_health();
                self.reason = Some(GuideReason::HealthPending);
                self.bump()?;
                return Err(GuideError::Invalid);
            }
        }
        self.bump()?;
        Ok(intents)
    }
    /// A local deadline retires the operation and requests detection; mutations are never resent.
    pub fn tick(&mut self, now: u64) -> Result<Vec<GuideIntent>, GuideError> {
        self.time(now)?;
        if self
            .pending
            .as_ref()
            .is_some_and(|p| now.saturating_sub(p.sent) >= MAX_TIMEOUT_MS)
        {
            let pending = self.pending.clone().ok_or(GuideError::Invalid)?;
            return self.reply(
                &pending.binding,
                AgentReply {
                    id: pending.call.id,
                    observed_at_ms: now,
                    source: self.admission.source,
                    result: Err(CallFailure::TimeoutOutcomeUnknown),
                },
                now,
            );
        }
        if self
            .pane
            .as_ref()
            .is_some_and(|(_, _, sent)| now.saturating_sub(*sent) >= MAX_TIMEOUT_MS)
        {
            let (binding, _, _) = self.pane.clone().ok_or(GuideError::Invalid)?;
            self.pane_completed(&binding, Err(NativeError::Timeout), now)?;
        }
        Ok(Vec::new())
    }
    pub fn select_permission(
        &mut self,
        token: &GuideToken,
        pane: SettingsPane,
        now: u64,
    ) -> Result<(), GuideError> {
        self.current(token, now)?;
        self.selected = pane;
        self.breadcrumbs = None;
        self.bump()
    }
    pub fn ask(&mut self, token: &GuideToken, now: u64) -> Result<Vec<GuideIntent>, GuideError> {
        self.current(token, now)?;
        self.ready()?;
        if self
            .required_rows()
            .iter()
            .filter(|row| row.required)
            .all(|row| row.state == PermissionState::Granted)
        {
            return Err(GuideError::NotReady);
        }
        self.enqueue(InstallerRequest::AskPermissions, now, false)
    }
    pub fn open_pane(
        &mut self,
        token: &GuideToken,
        now: u64,
    ) -> Result<Vec<GuideIntent>, GuideError> {
        self.current(token, now)?;
        if self.pane.is_some() {
            return Err(GuideError::Busy);
        }
        let binding = self.binding()?;
        let pane = self.selected;
        self.pane = Some((binding.clone(), pane, now));
        self.bump()?;
        Ok(vec![GuideIntent::OpenPane { binding, pane }])
    }
    pub fn pane_completed(
        &mut self,
        binding: &GuideBinding,
        result: NativeResult<()>,
        now: u64,
    ) -> Result<(), GuideError> {
        self.time(now)?;
        let (expected, pane, _) = self.pane.as_ref().ok_or(GuideError::Stale)?;
        if binding != expected || binding.token.instance != self.admission.instance.id {
            return Err(GuideError::Stale);
        }
        if result.is_err() {
            self.breadcrumbs = Some(pane.breadcrumbs());
            self.reason = Some(GuideReason::SettingsBreadcrumbs);
        }
        self.pane = None;
        self.bump()
    }
    /// Only a fresh user action can restart. The caller displays view.restart_warning before
    /// consent, and retires every readiness proof when receiving RetireActivity.
    pub fn restart(
        &mut self,
        token: &GuideToken,
        interruption_accepted: bool,
        now: u64,
    ) -> Result<Vec<GuideIntent>, GuideError> {
        self.current(token, now)?;
        if self.pending.is_some()
            || (self.waiting_instance.is_some()
                && !(self.restart_uncertain && self.health.as_deref().is_some_and(recovered)))
        {
            return Err(GuideError::Busy);
        }
        let h = self.health.as_deref().ok_or(GuideError::NotReady)?;
        if active(h) && !interruption_accepted {
            return Err(GuideError::NotReady);
        }
        let settings = self.settings_active
            && matches!(
                self.settings.state(),
                SettingsTransitionState::NeedsRestartConsent
                    | SettingsTransitionState::NeedsRecoveryRestartConsent
            );
        let mut intents = Vec::new();
        if settings {
            let samples = self.settings.current_samples().to_vec();
            let (events, _) = self
                .settings
                .consent_restart(self.next_id, self.settings.view_revision(), &[], &samples)
                .map_err(|_| GuideError::Invalid)?;
            intents.push(GuideIntent::Core(events));
        } else if self.settings_active
            && self.settings.state() != &SettingsTransitionState::Complete
        {
            return Err(GuideError::NotReady);
        }
        intents.push(self.retire_activity()?);
        self.waiting_instance = Some(self.admission.instance.id);
        self.restart_uncertain = false;
        self.reason = Some(GuideReason::StartupWaiting);
        intents.extend(self.enqueue(InstallerRequest::Restart, now, settings)?);
        Ok(intents)
    }
    pub fn explain_network(&mut self, token: &GuideToken, now: u64) -> Result<(), GuideError> {
        self.current(token, now)?;
        self.network_explained = true;
        self.bump()
    }
    pub fn report_network_denial(
        &mut self,
        token: &GuideToken,
        now: u64,
    ) -> Result<(), GuideError> {
        self.current(token, now)?;
        self.denied = true;
        self.bump()
    }
    /// Separate explicit retry; discovery facts alone never authorize a privacy claim.
    pub fn dial(
        &mut self,
        token: &GuideToken,
        addr: std::net::SocketAddr,
        now: u64,
    ) -> Result<Vec<GuideIntent>, GuideError> {
        self.current(token, now)?;
        self.ready()?;
        if !self.network_explained || addr.port() == 0 {
            return Err(GuideError::NotReady);
        }
        self.denied = false;
        self.enqueue(InstallerRequest::Dial { addr }, now, false)
    }
    pub fn choose_hiding(
        &mut self,
        token: &GuideToken,
        choice: HidingChoice,
        now: u64,
    ) -> Result<(), GuideError> {
        self.current(token, now)?;
        if self.settings_active {
            return Err(GuideError::Busy);
        }
        self.choice = Some(choice);
        self.parking = ParkingVerification::Pending;
        self.bump()
    }
    /// Uses the producer's loaded revision for CAS. An external disk edit produces conflict;
    /// recovery requires re-observation/restart/new consent, never a manufactured disk proof.
    pub fn commit_hiding(
        &mut self,
        token: &GuideToken,
        now: u64,
    ) -> Result<Vec<GuideIntent>, GuideError> {
        self.current(token, now)?;
        let h = self.ready()?.clone();
        if self.pending.is_some() {
            return Err(GuideError::Busy);
        }
        let hide = self.choice.ok_or(GuideError::NotReady)? == HidingChoice::Hide;
        let events = self
            .settings
            .detected(&h, self.admission.source, self.at, now, self.revision)
            .map_err(|_| GuideError::Invalid)?;
        let call = self
            .settings
            .consent_update(self.next_id, self.settings.view_revision(), hide)
            .map_err(|_| GuideError::NotReady)?;
        self.settings_active = true;
        let mut intents = vec![GuideIntent::Core(events)];
        intents.extend(self.enqueue(call.request, now, true)?);
        Ok(intents)
    }
    /// After a conflict, a new admitted observation gets a new consent view. If a disk edit
    /// is not loaded, the frozen SettingsTransition first asks for a recovery restart.
    pub fn redetect_hiding(
        &mut self,
        token: &GuideToken,
        now: u64,
    ) -> Result<Vec<GuideIntent>, GuideError> {
        self.current(token, now)?;
        let h = self.ready()?.clone();
        let events = self
            .settings
            .detected(&h, self.admission.source, self.at, now, self.revision)
            .map_err(|_| GuideError::Invalid)?;
        self.settings_active =
            !matches!(self.settings.state(), SettingsTransitionState::NeedsConsent);
        self.bump()?;
        Ok(vec![GuideIntent::Core(events)])
    }
    pub fn begin_projection_check(
        &mut self,
        token: &GuideToken,
        peer: NodeId,
        now: u64,
    ) -> Result<(), GuideError> {
        self.current(token, now)?;
        let h = self.ready()?;
        if self.settings.state() != &SettingsTransitionState::Complete || self.counter_invalid {
            return Err(GuideError::NotReady);
        }
        let p = h
            .installer()
            .peers
            .iter()
            .find(|p| p.node == peer && p.connected)
            .ok_or(GuideError::NotReady)?;
        self.attempt = Some(ProjectionAttempt {
            token: self.token(),
            peer,
            link: p.link_generation.ok_or(GuideError::NotReady)?,
            counter: p.counters.e2_source_started,
            epochs: h.installer().epochs.clone(),
            at: now,
        });
        self.parking = ParkingVerification::Pending;
        Ok(())
    }
    /// Call with the token issued at begin_projection_check, after the person's owned source
    /// exercise. Exactly one counter advance avoids attributing ambiguous concurrent starts.
    pub fn confirm_parking(
        &mut self,
        token: &GuideToken,
        human: ParkingObservation,
        now: u64,
    ) -> Result<ParkingVerification, GuideError> {
        self.time(now)?;
        let h = self.ready()?;
        let a = self.attempt.as_ref().ok_or(GuideError::NotReady)?;
        if token != &a.token
            || token.generation != self.generation
            || !fresh(self.at, now)
            || self.at <= a.at
            || h.installer().epochs != a.epochs
        {
            return Err(GuideError::Stale);
        }
        let p = h
            .installer()
            .peers
            .iter()
            .find(|p| p.node == a.peer && p.connected && p.link_generation == Some(a.link))
            .ok_or(GuideError::NotReady)?;
        if a.counter.checked_add(1) != Some(p.counters.e2_source_started) {
            return Ok(ParkingVerification::Pending);
        }
        self.parking = match (p.last_source_parking, human, self.choice) {
            (Some(SourceParking::Twin), ParkingObservation::Hidden, Some(HidingChoice::Hide)) => {
                ParkingVerification::Twin
            }
            (
                Some(SourceParking::Mirror),
                ParkingObservation::Visible,
                Some(HidingChoice::Hide),
            ) => ParkingVerification::MirrorFallback,
            (
                Some(SourceParking::Mirror),
                ParkingObservation::Visible,
                Some(HidingChoice::Mirror),
            ) => ParkingVerification::Mirror,
            _ => ParkingVerification::Pending,
        };
        self.bump()?;
        Ok(self.parking)
    }
    fn required_rows(&self) -> Vec<GrantRow> {
        let h = self.health.as_deref();
        let enabled = h.is_some_and(|h| h.installer().audio.enabled);
        [
            PermissionName::ScreenRecording,
            PermissionName::Accessibility,
            PermissionName::InputMonitoring,
            PermissionName::Microphone,
        ]
        .into_iter()
        .map(|permission| GrantRow {
            permission,
            required: permission != PermissionName::Microphone || enabled,
            state: h
                .and_then(|h| {
                    h.installer()
                        .permissions
                        .iter()
                        .find(|p| p.name == permission)
                        .map(|p| p.state)
                })
                .unwrap_or(PermissionState::Unknown),
        })
        .collect()
    }
    pub fn view(&self, now: u64) -> GuideView {
        let usable = fresh(self.at, now)
            && self.health.is_some()
            && self.waiting_instance.is_none()
            && self.reason != Some(GuideReason::MigrationPending);
        let mut rows = self.required_rows();
        if !usable {
            for row in &mut rows {
                row.state = PermissionState::Unknown;
            }
        }
        let audio_prerequisite = usable
            && self.admission.source == ObservationSource::Live
            && self.ready().is_ok_and(|h| h.installer().audio.enabled)
            && rows.iter().all(|r| r.state == PermissionState::Granted);
        let network = if !self.network_explained {
            NetworkState::Explanation
        } else if self.denied {
            NetworkState::ReportedDenied
        } else if !usable {
            NetworkState::UnknownConnectivity
        } else {
            let i = self.health.as_deref().map(|h| h.installer());
            match i {
                Some(i) if i.peers.iter().any(|p| p.connected) => NetworkState::Connected,
                Some(i)
                    if i.discovery.enabled
                        && i.discovery.running
                        && i.discovery.error.is_none() =>
                {
                    NetworkState::WaitingForPeer
                }
                _ => NetworkState::UnknownConnectivity,
            }
        };
        GuideView {
            token: self.token(),
            screen: if self.selected == SettingsPane::Microphone {
                ScreenId::AudioComponent
            } else {
                ScreenId::Permissions
            },
            rows,
            audio_prerequisite,
            reason: if !usable && self.reason.is_none() {
                Some(GuideReason::HealthPending)
            } else {
                self.reason
            },
            breadcrumbs: if self.denied {
                Some("System Settings > Privacy & Security > Local Network > Crosspane")
            } else {
                self.breadcrumbs.or(Some(self.selected.breadcrumbs()))
            },
            restart_warning: if self.health.as_deref().is_some_and(active) {
                "Restart Crosspane ends active control, projections and speaker audio. Continue only after accepting that interruption."
            } else {
                "Restart Crosspane and wait for its ordinary startup recovery and a new signed agent instance."
            },
            network,
            hiding_choice: self.choice,
            hiding_next_enabled: self.choice.is_some()
                && (!self.settings_active
                    || (usable
                        && self.ready().is_ok()
                        && self.parking != ParkingVerification::Pending)),
            parking: if usable && self.ready().is_ok() {
                self.parking
            } else {
                ParkingVerification::Pending
            },
            simulated: self.admission.source != ObservationSource::Live,
        }
    }
}
