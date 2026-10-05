//! Read-only detection data and pure decisions. These reports confer no mutation or readiness
//! authority. Native acquisition and support-proof assembly belong to WP-4.8b2; runtime/fonts
//! are registered by WP-4.8c once their real implementations land.
pub mod fonts;
mod probe;
mod report;
pub mod runtime;
mod session;
pub use crate::agent_contract::ObservationSource;
use crate::agent_contract::{
    AgentReply, BootstrapV1, DecodedReply, KeyStoreProvenance, StatusAdmission,
};
pub use probe::*;
pub use report::*;
pub use session::*;
use std::path::PathBuf;

/// Missing/WrongVersion are established negative facts. Native Unavailable never proves either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeIssue {
    Missing,
    WrongVersion,
    Unavailable,
    Timeout,
    Cancelled,
    Oversize,
    Malformed,
    Foreign,
    Ambiguous,
    Unverified,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Fact<T> {
    pub value: Result<T, ProbeIssue>,
    pub source: ObservationSource,
    /// Original receipt/conversion time in the caller's monotonic clock; never delivery time.
    pub observed_at_ms: u64,
}
impl<T> Fact<T> {
    pub fn known(value: T, source: ObservationSource, observed_at_ms: u64) -> Self {
        Self {
            value: Ok(value),
            source,
            observed_at_ms,
        }
    }
    pub fn issue(issue: ProbeIssue, source: ObservationSource, observed_at_ms: u64) -> Self {
        Self {
            value: Err(issue),
            source,
            observed_at_ms,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OsFamily {
    Arch,
    Other(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Architecture {
    X86_64,
    Aarch64,
    Other(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveEnvironment {
    pub runtime_dir: PathBuf,
    pub wayland_display: String,
    pub hyprland_instance_signature: String,
    pub session_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct SessionFacts {
    pub uid: u32,
    pub os: Fact<OsFamily>,
    pub architecture: Fact<Architecture>,
    pub hyprland_version: Fact<[u16; 3]>,
    pub protocols: Fact<bool>,
    /// Actual lifecycle evidence, not a unit name, exported flag, or active target alone.
    pub uwsm_managed: Fact<bool>,
    pub graphical_target_active: Fact<bool>,
    pub graphical_sessions: Fact<usize>,
    pub selected_session: Fact<Option<SelectedSession>>,
    pub selected_environment: EffectiveEnvironment,
    pub manager_environment: Fact<EffectiveEnvironment>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct LibraryFact {
    /// Bounded bare SONAME. Resolution is structural, never load/ABI proof.
    pub name: String,
    pub required: bool,
    pub resolved: Fact<PathBuf>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeFacts {
    pub libraries: Vec<LibraryFact>,
    /// Complete structural dependency traversal; never a load/ABI readiness claim.
    pub dependency_graph: Fact<bool>,
    pub video_feature: Fact<bool>,
    pub ffmpeg: Fact<bool>,
    pub opus: Fact<bool>,
    pub pipewire_library: Fact<bool>,
    pub xkb: Fact<bool>,
    pub wayland_library: Fact<bool>,
    pub software_video: Fact<bool>,
    pub gpu: Fact<bool>,
    pub libei_required: bool,
    /// Unverified until matched backend facts or attended checks, never invented bus names.
    pub pipewire: Fact<bool>,
    pub session_manager: Fact<bool>,
    pub secret_service: Fact<bool>,
    pub keystore: Fact<KeyStoreProvenance>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct InstalledAgentFacts {
    pub bootstrap: BootstrapV1,
    pub status: StatusAdmission,
    pub call_id: u64,
    pub received_at_ms: u64,
    pub source: ObservationSource,
}
impl InstalledAgentFacts {
    /// Pure wire correlation only. WP-4.8b2 must additionally admit PID/start/executable/endpoint
    /// against the selected native target before calling an installed-agent observation known.
    pub fn from_reply(bootstrap: BootstrapV1, reply: AgentReply) -> Result<Self, ProbeIssue> {
        let status = match reply.result {
            Ok(DecodedReply::Status(status)) => status,
            _ => return Err(ProbeIssue::Unverified),
        };
        if let StatusAdmission::Supported(health) = &status {
            let instance = &health.installer().instance;
            if instance.id != bootstrap.instance_id
                || instance.pid != bootstrap.pid
                || instance.started_unix_ms != bootstrap.started_unix_ms
                || instance.runtime_dir != bootstrap.runtime_dir
            {
                return Err(ProbeIssue::Foreign);
            }
        }
        Ok(Self {
            bootstrap,
            status,
            call_id: reply.id,
            received_at_ms: reply.observed_at_ms,
            source: reply.source,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedReason {
    OperatingSystem,
    Architecture,
    HyprlandVersion,
    RequiredProtocols,
    Uwsm,
    SessionType,
    VideoFeature,
    RuntimeLibrary,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eligibility {
    Supported,
    NotSupported(UnsupportedReason),
    Pending(ProbeIssue),
}
#[derive(Clone, Debug, PartialEq)]
pub struct SupportReport {
    pub eligibility: Eligibility,
    pub session: SessionFacts,
    pub runtime: RuntimeFacts,
    pub installed_agent: Fact<InstalledAgentFacts>,
    /// Unknown makes shell Auto choose Reduced. No settings-daemon dependency or guessed value.
    pub reduced_motion: Fact<bool>,
}

/// Compatibility is evidence alongside mutation authority, never a substitute for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompatibilityReport {
    pub eligibility: Eligibility,
    pub notes: Vec<String>,
}

impl Default for CompatibilityReport {
    fn default() -> Self {
        Self {
            eligibility: Eligibility::Pending(ProbeIssue::Unverified),
            notes: vec!["Compatibility has not been observed.".into()],
        }
    }
}

/// The session facts that authorize selected-user operations. Compatibility issues are not
/// consulted here; missing identity, lifecycle or environment evidence still refuses.
pub fn session_authority(session: &SessionFacts) -> Result<(), ProbeIssue> {
    let required = |fact: &Fact<bool>| match fact.value {
        Ok(true) => Ok(()),
        Ok(false) => Err(ProbeIssue::Unverified),
        Err(issue) => Err(issue),
    };
    required(&session.uwsm_managed)?;
    required(&session.graphical_target_active)?;
    match session.graphical_sessions.value {
        Ok(1) => {}
        Ok(_) => return Err(ProbeIssue::Ambiguous),
        Err(issue) => return Err(issue),
    }
    let selected = session
        .selected_session
        .value
        .as_ref()
        .map_err(|issue| *issue)?
        .as_ref()
        .ok_or(ProbeIssue::Ambiguous)?;
    let chosen = &selected.session;
    if chosen.uid != Some(session.uid) {
        return Err(ProbeIssue::Foreign);
    }
    if chosen.kind.as_deref() != Some("wayland")
        || chosen.active != Some(true)
        || chosen.seat.as_deref().is_none_or(str::is_empty)
    {
        return Err(ProbeIssue::Unverified);
    }
    let effective = session
        .manager_environment
        .value
        .as_ref()
        .map_err(|issue| *issue)?;
    if effective.runtime_dir != session.selected_environment.runtime_dir
        || effective.wayland_display != session.selected_environment.wayland_display
        || effective.hyprland_instance_signature
            != session.selected_environment.hyprland_instance_signature
    {
        return Err(ProbeIssue::Foreign);
    }
    Ok(())
}

pub fn compatibility_report(session: &SessionFacts, runtime: &RuntimeFacts) -> CompatibilityReport {
    let mut notes = Vec::new();
    let mut note = |label: &str, issue: Option<ProbeIssue>| {
        if let Some(issue) = issue {
            notes.push(format!("Couldn't confirm {label}: saw {issue:?}."));
        }
    };
    note("operating system", session.os.value.as_ref().err().copied());
    note(
        "architecture",
        session.architecture.value.as_ref().err().copied(),
    );
    note("Hyprland version", session.hyprland_version.value.err());
    for (label, fact) in [
        ("required protocols", &session.protocols),
        ("dependency graph", &runtime.dependency_graph),
        ("video feature", &runtime.video_feature),
        ("FFmpeg libraries", &runtime.ffmpeg),
        ("Opus library", &runtime.opus),
        ("PipeWire library", &runtime.pipewire_library),
        ("keyboard library", &runtime.xkb),
        ("Wayland library", &runtime.wayland_library),
        ("software video", &runtime.software_video),
        ("optional GPU", &runtime.gpu),
        ("optional audio", &runtime.pipewire),
        ("optional session manager", &runtime.session_manager),
        ("secret service", &runtime.secret_service),
    ] {
        note(label, fact.value.err());
    }
    for library in &runtime.libraries {
        if library.resolved.value.is_err() {
            // A native SONAME may be untrusted. The issue and count suffice; never echo it.
            note(
                "a runtime dependency",
                library.resolved.value.as_ref().err().copied(),
            );
        }
    }
    let eligibility = classify(session, runtime);
    if let Eligibility::NotSupported(reason) = eligibility {
        notes.push(unsupported_text(reason));
    }
    CompatibilityReport { eligibility, notes }
}

/// Structural eligibility only: never WorkspaceReady, backend-load proof, an open gate, usable
/// audio, store unlock, or next-login evidence. Optional GPU and nonapplicable libei are ignored.
pub fn classify(session: &SessionFacts, runtime: &RuntimeFacts) -> Eligibility {
    use UnsupportedReason as U;
    let mut pending = None;
    let checks = [
        (
            session.os.value.as_ref().map(|v| *v == OsFamily::Arch),
            U::OperatingSystem,
        ),
        (
            session
                .architecture
                .value
                .as_ref()
                .map(|v| matches!(v, Architecture::X86_64 | Architecture::Aarch64)),
            U::Architecture,
        ),
        (
            session
                .hyprland_version
                .value
                .as_ref()
                .map(|v| *v >= [0, 56, 0]),
            U::HyprlandVersion,
        ),
    ];
    let booleans = [
        (&session.protocols, U::RequiredProtocols),
        (&session.uwsm_managed, U::Uwsm),
        (&runtime.dependency_graph, U::RuntimeLibrary),
        (&runtime.video_feature, U::VideoFeature),
        (&runtime.ffmpeg, U::RuntimeLibrary),
        (&runtime.opus, U::RuntimeLibrary),
        (&runtime.pipewire_library, U::RuntimeLibrary),
        (&runtime.xkb, U::RuntimeLibrary),
        (&runtime.wayland_library, U::RuntimeLibrary),
        (&runtime.software_video, U::RuntimeLibrary),
    ];
    for (value, reason) in checks.into_iter().chain(
        booleans
            .into_iter()
            .map(|(fact, reason)| (fact.value.as_ref().copied(), reason)),
    ) {
        match value {
            Ok(false) => return Eligibility::NotSupported(reason),
            Err(issue) => {
                pending.get_or_insert(*issue);
            }
            Ok(true) => {}
        }
    }
    for library in runtime.libraries.iter().filter(|v| v.required) {
        if let Err(issue) = library.resolved.value {
            if matches!(issue, ProbeIssue::Missing | ProbeIssue::WrongVersion) {
                return Eligibility::NotSupported(U::RuntimeLibrary);
            }
            pending.get_or_insert(issue);
        }
    }
    if !runtime.libraries.iter().any(|v| v.required) || runtime.libei_required {
        pending.get_or_insert(ProbeIssue::Unverified);
    }
    let chosen = match &session.selected_session.value {
        Ok(Some(chosen)) => &chosen.session,
        Ok(None) => return Eligibility::Pending(ProbeIssue::Ambiguous),
        Err(issue) => return Eligibility::Pending(*issue),
    };
    if chosen.kind.is_none() {
        return Eligibility::Pending(ProbeIssue::Unverified);
    }
    if chosen.kind.as_deref() != Some("wayland") {
        return Eligibility::NotSupported(U::SessionType);
    }
    if chosen.uid != Some(session.uid) {
        return Eligibility::Pending(ProbeIssue::Foreign);
    }
    if chosen.seat.as_deref().is_none_or(str::is_empty) || chosen.active != Some(true) {
        pending.get_or_insert(ProbeIssue::Unverified);
    }
    if session.graphical_target_active.value != Ok(true) {
        pending.get_or_insert(
            session
                .graphical_target_active
                .value
                .err()
                .unwrap_or(ProbeIssue::Unverified),
        );
    }
    if session.graphical_sessions.value != Ok(1) {
        pending.get_or_insert(
            session
                .graphical_sessions
                .value
                .err()
                .unwrap_or(ProbeIssue::Ambiguous),
        );
    }
    match &session.manager_environment.value {
        Ok(effective)
            if effective.runtime_dir == session.selected_environment.runtime_dir
                && effective.wayland_display == session.selected_environment.wayland_display
                && effective.hyprland_instance_signature
                    == session.selected_environment.hyprland_instance_signature => {}
        Ok(_) => {
            pending.get_or_insert(ProbeIssue::Foreign);
        }
        Err(issue) => {
            pending.get_or_insert(*issue);
        }
    }
    pending.map_or(Eligibility::Supported, Eligibility::Pending)
}

pub fn unsupported_text(reason: UnsupportedReason) -> String {
    match reason {
        UnsupportedReason::OperatingSystem => {
            "This installer supports Arch-based Linux, such as Omarchy, for now.".into()
        }
        UnsupportedReason::Architecture => {
            "This processor isn't supported by this installer yet.".into()
        }
        UnsupportedReason::HyprlandVersion => {
            "Crosspane needs Hyprland 0.56 or newer on this computer.".into()
        }
        UnsupportedReason::RequiredProtocols => {
            "This Hyprland session doesn't offer the Wayland features Crosspane needs.".into()
        }
        UnsupportedReason::Uwsm => {
            "Setup supports Hyprland sessions managed by uwsm for now. Yours isn't.".into()
        }
        UnsupportedReason::SessionType => {
            "Crosspane needs a Wayland session. This one is something else.".into()
        }
        UnsupportedReason::VideoFeature => {
            "The staged Crosspane build doesn't include video support.".into()
        }
        UnsupportedReason::RuntimeLibrary => {
            "A library Crosspane needs isn't installed on this computer.".into()
        }
    }
}
