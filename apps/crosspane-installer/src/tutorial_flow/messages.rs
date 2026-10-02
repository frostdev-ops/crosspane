use crate::agent_contract::{AgentCall, AgentPlatform, AgentReply, CallFailure};
use crosspane_installer_core::{
    AttemptId, EvidenceBinding, EvidenceError, FlowError, FlowEvent, JobIntent, OperationId, StepId,
};
use crosspane_types::id::{NodeId, WindowId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TutorialRole {
    E1Controller,
    E1Target,
    E2SourcePush,
    E2DestinationPush,
    E2SourcePull,
    E2DestinationPull,
    AudioSender,
    AudioReceiver,
    Menu,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TutorialSourcePolicy {
    Native,
    MacMirror,
    MacPrivateDisplay,
}
#[derive(Clone, PartialEq)]
pub struct TutorialSpeakers {
    pub peer: NodeId,
    pub device_key: String,
}
#[derive(Clone, PartialEq)]
pub struct TutorialContext {
    pub machine_label: String,
    pub platform: AgentPlatform,
    pub source_policy: TutorialSourcePolicy,
    pub speakers: Option<TutorialSpeakers>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TutorialAttempt {
    pub step: StepId,
    pub operation: OperationId,
    pub attempt: AttemptId,
    pub local: NodeId,
    pub peer: Option<NodeId>,
    pub role: TutorialRole,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TutorialState {
    Idle,
    WaitingHealth,
    WaitingFixture,
    WaitingUser,
    Running,
    WaitingTerminal,
    PendingContract,
    Failed,
    Verified,
    Cancelled,
}
#[derive(Clone, PartialEq)]
pub enum TutorialFixtureAction {
    Open {
        machine_label: String,
    },
    ArmTarget {
        fixture: u64,
        phase: u64,
    },
    ObserveWindow {
        fixture: u64,
    },
    PlayTone {
        fixture: u64,
        peer: NodeId,
        device_key: String,
    },
    StopTone {
        fixture: u64,
        tone: u64,
    },
    Close {
        fixture: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TutorialWindowFacts {
    Unknown,
    Present {
        visible_on_user_workspace: Option<bool>,
        on_initial_display: Option<bool>,
    },
    Missing,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TutorialToneState {
    Stopped,
    Running { tone: u64 },
    StopUnconfirmed { tone: u64 },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("fixture outcome: {self:?}")]
pub enum TutorialFixtureError {
    BadCall,
    Busy,
    NotOwned,
    Unavailable,
    UnknownWindow,
    AmbiguousWindow,
    OutputUnavailable,
    OutputChanged,
    UnsupportedFormat,
    TimedOut,
    ChildExited,
    CounterExhausted,
    ChannelClosed,
    InvalidMessage,
    Refused,
    CleanupFailed,
}
#[derive(Clone, PartialEq)]
pub enum TutorialFixtureObservation {
    Opened {
        fixture: u64,
        pid: u32,
        window: WindowId,
        label: String,
    },
    TargetArmed {
        fixture: u64,
        phase: u64,
    },
    Snapshot {
        fixture: u64,
        window: WindowId,
        phase: Option<u64>,
        pattern_ticks: u64,
        target_clicks: u64,
        window_facts: TutorialWindowFacts,
        tone: TutorialToneState,
    },
    ToneStarted {
        fixture: u64,
        tone: u64,
    },
    ToneStopped {
        fixture: u64,
        tone: u64,
    },
    CloseRequested {
        fixture: u64,
    },
    Closed {
        fixture: u64,
    },
    Lost {
        fixture: u64,
        reason: TutorialFixtureError,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HumanConfirmation {
    RemotePracticeAndHud,
    ControllerCrossingAndRelease,
    DestinationPatternInteractionAndClose,
    SourceRestored,
    FarSpeakerHeard,
    /// Local hearing, including confirmation of the selected sending machine.
    LocalSpeakerHeard,
    TrayAndSettingsVisible,
    ExclusiveAudioInterval,
    /// The person matched the source machine and this attempt's fixture label.
    SourceMachineAndAttempt,
    /// The person observed the local source hidden on its twin display.
    PrivateDisplayObserved,
    /// Human attestation of an explicit fixture-tone action on the selected source machine.
    SelectedSourceToneStarted,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TutorialUserAction {
    Confirm(HumanConfirmation),
    PlayTestSound,
    SelectRemoteWindow { window: WindowId },
    Cancel,
}
#[derive(Clone, PartialEq)]
pub enum TutorialEvent {
    Reply(AgentReply),
    Fixture {
        attempt: AttemptId,
        call_id: Option<u64>,
        sequence: u64,
        observed_at_ms: u64,
        result: Result<TutorialFixtureObservation, TutorialFixtureError>,
    },
    FixtureSubmitFailed {
        attempt: AttemptId,
        call_id: u64,
        error: TutorialFixtureError,
    },
    User {
        attempt: AttemptId,
        view_revision: u64,
        action: TutorialUserAction,
    },
    CoreJob(JobIntent),
    CoreRejected(FlowError),
    Tick,
}
/// Every effect carries the immutable selection and the last admitted instance/link/epochs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TutorialBinding {
    pub attempt: TutorialAttempt,
    pub view_revision: u64,
    pub evidence: Option<EvidenceBinding>,
}
#[derive(Clone, PartialEq)]
pub struct TutorialEffect {
    pub binding: TutorialBinding,
    pub kind: TutorialEffectKind,
}
#[derive(Clone, PartialEq)]
pub enum TutorialEffectKind {
    Core(FlowEvent),
    Agent(AgentCall),
    Fixture {
        call_id: u64,
        action: TutorialFixtureAction,
    },
    WaitForUser,
    WaitForPeer,
    WaitForContract,
    DetectAfterUnknown,
}
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("tutorial correlation: {self:?}")]
pub enum TutorialError {
    Busy,
    InvalidAttempt,
    WrongOperation,
    WrongRole,
    WrongPeer,
    RetiredView,
    InvalidObservation,
    IdExhausted,
    InsufficientContract,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TutorialFailure {
    Agent(CallFailure),
    Fixture(TutorialFixtureError),
    Evidence(EvidenceError),
    Core(FlowError),
    Health,
    Isolation,
    BindingChanged,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkingResult {
    Native,
    Twin,
    MirrorFallback,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteRestoration {
    NotApplicable,
    Unknown,
    HumanConfirmed,
}
/// Detail-drawer facts, not a continuously measured guarantee or a readiness flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TutorialDetail {
    pub failure: Option<TutorialFailure>,
    pub parking: ParkingResult,
    pub private_display_verified: bool,
    pub global_audio_counters: bool,
    pub sampled_peer_membership: bool,
    pub human_hearing_required: bool,
    pub unseen_between_poll_audio_possible: bool,
    pub backend_changes_within_one_tick_may_be_missed: bool,
    pub held_ledger_settlement_observed: bool,
    pub local_cleanup_settled: bool,
    pub remote_restoration: RemoteRestoration,
    pub cleanup_settled: bool,
}
impl Default for TutorialDetail {
    fn default() -> Self {
        Self {
            failure: None,
            parking: ParkingResult::Unknown,
            private_display_verified: false,
            global_audio_counters: true,
            sampled_peer_membership: true,
            human_hearing_required: true,
            unseen_between_poll_audio_possible: true,
            backend_changes_within_one_tick_may_be_missed: true,
            held_ledger_settlement_observed: false,
            local_cleanup_settled: false,
            remote_restoration: RemoteRestoration::NotApplicable,
            cleanup_settled: false,
        }
    }
}
// Labels, device identifiers and observation text never appear in diagnostics.
macro_rules! redacted_debug {
    ($($ty:ty),+ $(,)?) => {$(
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(stringify!($ty))
            }
        }
    )+};
}
redacted_debug!(
    TutorialContext,
    TutorialSpeakers,
    TutorialFixtureAction,
    TutorialFixtureObservation,
    TutorialEvent,
    TutorialEffect,
    TutorialEffectKind
);
