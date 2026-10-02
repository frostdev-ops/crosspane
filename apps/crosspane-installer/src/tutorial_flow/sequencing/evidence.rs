use super::*;

pub(super) fn bounded(s: &str, bytes: usize) -> bool {
    s.len() <= bytes && !s.chars().any(char::is_control)
}
pub(super) fn source_role(r: TutorialRole) -> bool {
    matches!(r, TutorialRole::E2SourcePush | TutorialRole::E2SourcePull)
}
pub(super) fn e2_role(r: TutorialRole) -> bool {
    source_role(r)
        || matches!(
            r,
            TutorialRole::E2DestinationPush | TutorialRole::E2DestinationPull
        )
}
pub(super) fn audio_role(r: TutorialRole) -> bool {
    matches!(r, TutorialRole::AudioSender | TutorialRole::AudioReceiver)
}
pub(super) fn uses_fixture(r: TutorialRole) -> bool {
    source_role(r) || matches!(r, TutorialRole::E1Target | TutorialRole::AudioSender)
}
pub(super) fn dependencies(role: TutorialRole) -> EpochDependencies {
    EpochDependencies {
        gate: true,
        grants: role != TutorialRole::Menu,
        layout: !audio_role(role) && role != TutorialRole::Menu,
        backends: true,
    }
}
pub(super) fn same_binding(
    a: &crosspane_installer_core::EvidenceBinding,
    b: &crosspane_installer_core::EvidenceBinding,
    d: EpochDependencies,
) -> bool {
    a.local_node == b.local_node
        && a.peer == b.peer
        && a.link_generation == b.link_generation
        && a.epochs.instance_id == b.epochs.instance_id
        && (!d.gate || a.epochs.gate_epoch == b.epochs.gate_epoch)
        && (!d.grants || a.epochs.grants_epoch == b.epochs.grants_epoch)
        && (!d.layout || a.epochs.layout_epoch == b.epochs.layout_epoch)
        && (!d.backends || a.epochs.backends_epoch == b.epochs.backends_epoch)
}
pub(super) fn delta(start: &CounterSample, end: &CounterSample, metric: Metric) -> Option<u64> {
    end.values
        .get(&CounterId(metric as u16))?
        .checked_sub(*start.values.get(&CounterId(metric as u16))?)
}
pub(super) fn requirements(role: TutorialRole) -> (Vec<CounterId>, Vec<CounterId>) {
    use Metric::*;
    let advance: &[Metric] = match role {
        TutorialRole::E1Controller => &[ControllerStarted, ControllerEnded, ChordReleases],
        TutorialRole::E1Target => &[TargetStarted, TargetEnded, InjectionsOk, HudShows],
        TutorialRole::E2SourcePush | TutorialRole::E2SourcePull => &[SourceStarted, SourceReturned],
        TutorialRole::E2DestinationPush | TutorialRole::E2DestinationPull => {
            &[DestStarted, DestReturned, FramesPresented]
        }
        TutorialRole::AudioSender => &[GlobalAudioSent],
        TutorialRole::AudioReceiver => &[GlobalAudioPlayed],
        TutorialRole::Menu => &[SettingsOpened],
    };
    let forbidden = (0..=16)
        .filter(|id| *id != FramesPresented as u16)
        .filter(|id| !advance.iter().any(|m| *m as u16 == *id))
        .filter(|id| role != TutorialRole::Menu || *id >= 14)
        .map(CounterId)
        .collect();
    (
        advance.iter().map(|m| CounterId(*m as u16)).collect(),
        forbidden,
    )
}
pub(super) fn isolated(role: TutorialRole, start: &CounterSample, end: &CounterSample) -> bool {
    if requirements(role)
        .1
        .iter()
        .any(|id| start.values.get(id) != end.values.get(id))
    {
        return false;
    }
    use Metric::*;
    let exact: &[Metric] = match role {
        TutorialRole::E1Controller => &[ControllerStarted, ControllerEnded, ChordReleases],
        TutorialRole::E1Target => &[TargetStarted, TargetEnded],
        TutorialRole::E2SourcePush | TutorialRole::E2SourcePull => &[SourceStarted, SourceReturned],
        TutorialRole::E2DestinationPush | TutorialRole::E2DestinationPull => {
            &[DestStarted, DestReturned]
        }
        _ => &[],
    };
    exact
        .iter()
        .all(|m| delta(start, end, *m).is_some_and(|n| n <= 1))
}
pub(super) fn confirmations(role: TutorialRole) -> &'static [HumanConfirmation] {
    use HumanConfirmation::*;
    match role {
        TutorialRole::E1Controller => &[RemotePracticeAndHud],
        TutorialRole::E1Target => &[ControllerCrossingAndRelease],
        TutorialRole::E2SourcePush | TutorialRole::E2SourcePull => {
            &[DestinationPatternInteractionAndClose]
        }
        TutorialRole::E2DestinationPush | TutorialRole::E2DestinationPull => &[
            DestinationPatternInteractionAndClose,
            SourceRestored,
            SourceMachineAndAttempt,
        ],
        TutorialRole::AudioSender => &[FarSpeakerHeard, ExclusiveAudioInterval],
        TutorialRole::AudioReceiver => &[
            LocalSpeakerHeard,
            ExclusiveAudioInterval,
            SelectedSourceToneStarted,
        ],
        TutorialRole::Menu => &[TrayAndSettingsVisible],
    }
}
pub(super) fn healthy(
    h: &HealthSnapshot,
    role: TutorialRole,
    platform: AgentPlatform,
    active_source: bool,
    peer: Option<NodeId>,
) -> bool {
    let i = h.installer();
    if !i.gate.open
        || i.gate.panic
        || i.gate.session != SessionState::Unlocked
        || i.gate.active != Some(true)
        || i.keystore != KeyStoreProvenance::OsStore
        || i.startup_recovery == StartupRecovery::Failed
        || (!active_source && i.recovery_pending != 0)
    {
        return false;
    }
    use BackendName::*;
    let required: &[BackendName] = match role {
        TutorialRole::E1Controller => &[Capture, Hotkeys, Links, Keystore],
        TutorialRole::E1Target => &[Keys, Pointer, Overlay, Links, Keystore],
        TutorialRole::E2SourcePush | TutorialRole::E2SourcePull => {
            &[Windows, Parking, Capture, Links, Keystore]
        }
        TutorialRole::E2DestinationPush | TutorialRole::E2DestinationPull => {
            &[Frames, Gpu, Links, Keystore]
        }
        TutorialRole::AudioSender | TutorialRole::AudioReceiver => &[Audio, Links, Keystore],
        TutorialRole::Menu => &[Tray, Keystore],
    };
    if !required.iter().all(|n| {
        i.backends
            .iter()
            .any(|b| b.name == *n && b.state == BackendState::Ready)
    }) {
        return false;
    }
    if platform == AgentPlatform::Macos
        && !i
            .permissions
            .iter()
            .filter(|p| p.name != PermissionName::Microphone || audio_role(role))
            .all(|p| p.state == PermissionState::Granted)
    {
        return false;
    }
    if audio_role(role)
        && (!i.audio.enabled
            || (platform == AgentPlatform::Macos
                && !i.permissions.iter().any(|p| {
                    p.name == PermissionName::Microphone && p.state == PermissionState::Granted
                })))
    {
        return false;
    }
    if role == TutorialRole::Menu {
        return i.tray.created;
    }
    let capability = match role {
        TutorialRole::E1Target => Some(Capability::Input),
        TutorialRole::E2SourcePush => Some(Capability::Share),
        TutorialRole::E2SourcePull => Some(Capability::Browse),
        TutorialRole::E2DestinationPush => Some(Capability::Present),
        TutorialRole::AudioReceiver => Some(Capability::Speaker),
        _ => None,
    };
    capability.is_none_or(|c| {
        i.peers
            .iter()
            .any(|p| Some(p.node) == peer && p.connected && p.grants_given.contains(&c))
    })
}
