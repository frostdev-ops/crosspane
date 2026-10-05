use super::probe::issue;
use super::*;
use crate::{
    agent_contract::{
        BackendFact, BackendName, BackendState, InstallerStatusV1, StartupRecovery, StatusAdmission,
    },
    platform::linux::native_io::*,
};
#[derive(Clone, Debug, PartialEq)]
pub enum BackendReadiness {
    Ready,
    Pending(Vec<BackendFact>),
    NotReady(Vec<BackendFact>),
    Recovery {
        pending: u32,
        startup: StartupRecovery,
    },
    Invalid,
}
/// Exact frozen 4.5 order; optional GPU/tray/audio/discovery and Linux Home do not block.
pub fn backend_readiness(status: &InstallerStatusV1) -> BackendReadiness {
    use BackendName::{Audio, Discovery, Gpu, Home, Tray};
    let facts = &status.backends;
    if facts.len() != 15 || facts.iter().enumerate().any(|(n, f)| f.name as usize != n) {
        return BackendReadiness::Invalid;
    }
    let optional = [Gpu, Tray, Audio, Discovery, Home];
    let required = |f: &&BackendFact| !optional.contains(&f.name);
    let failed: Vec<_> = facts
        .iter()
        .filter(required)
        .filter(|f| matches!(f.state, BackendState::Missing | BackendState::Failed))
        .cloned()
        .collect();
    if !failed.is_empty() {
        return BackendReadiness::NotReady(failed);
    }
    let blocked: Vec<_> = facts
        .iter()
        .filter(required)
        .filter(|f| f.state == BackendState::Blocked)
        .cloned()
        .collect();
    if !blocked.is_empty() {
        return BackendReadiness::Pending(blocked);
    }
    if status.recovery_pending != 0 || status.startup_recovery == StartupRecovery::Failed {
        return BackendReadiness::Recovery {
            pending: status.recovery_pending,
            startup: status.startup_recovery,
        };
    }
    BackendReadiness::Ready
}
/// One acquired pass. Inner facts retain their individual receipt times and sources.
#[derive(Debug)]
pub struct DetectionPass {
    pub os: OsFacts,
    pub architecture: Fact<Architecture>,
    pub logind: Fact<LogindFacts>,
    pub manager: Fact<ManagerFacts>,
    pub manager_environment: Fact<EffectiveEnvironment>,
    pub hyprland: Fact<HyprlandFacts>,
    pub registry: Fact<RegistryFacts>,
    /// Launcher evidence, consulted only when uwsm's MainPID differs from the Hyprland peer.
    pub lineage: Fact<CompositorLineage>,
    pub installed_agent: Fact<InstalledAgentFacts>,
    pub reduced_motion: Fact<bool>,
}
#[derive(Debug)]
pub struct DetectionResult {
    pub report: SupportReport,
    pub os_path: Option<PathBuf>,
    pub backends: Fact<BackendReadiness>,
    /// Structural mutation admission only; never WorkspaceReady or attended release evidence.
    pub proof: Option<SupportProof>,
}
fn field<T, U>(fact: Fact<T>, take: impl FnOnce(T) -> Fact<U>) -> Fact<U> {
    match fact.value {
        Ok(value) => take(value),
        Err(error) => Fact::issue(error, fact.source, fact.observed_at_ms),
    }
}
/// Injected composition admits proofs only for explicit scratch targets. Native callers use
/// NativeSessionProbes::detect. Evidence is never re-stamped; runtime acquisition belongs to c.
pub fn assemble_support(
    io: &LinuxNativeIo,
    selected_environment: EffectiveEnvironment,
    pass: DetectionPass,
    runtime: RuntimeFacts,
    deadline: &Deadline,
) -> DetectionResult {
    compose_support(
        io,
        selected_environment,
        pass,
        runtime,
        deadline,
        io.target().source() == ObservationSource::Demo,
    )
}
pub(super) fn compose_support(
    io: &LinuxNativeIo,
    selected_environment: EffectiveEnvironment,
    pass: DetectionPass,
    runtime: RuntimeFacts,
    deadline: &Deadline,
    admitted_pass: bool,
) -> DetectionResult {
    let correlation = match (
        &pass.manager.value,
        &pass.hyprland.value,
        &pass.registry.value,
    ) {
        (Ok(manager), Ok(hyprland), Ok(registry)) => match manager.compositor_pid {
            Some(_) if hyprland.pid != registry.pid => Err(ProbeIssue::Foreign),
            Some(pid) => compositor_matches(pid, hyprland.pid, &pass.lineage.value),
            None => Err(ProbeIssue::Unverified),
        },
        (Err(issue), _, _) | (_, Err(issue), _) | (_, _, Err(issue)) => Err(*issue),
    };
    let backends = Fact {
        value: pass
            .installed_agent
            .value
            .as_ref()
            .map_err(|e| *e)
            .and_then(|agent| match &agent.status {
                StatusAdmission::Supported(health) => Ok(backend_readiness(health.installer())),
                StatusAdmission::PendingHealthContract(_) => Err(ProbeIssue::Unverified),
            }),
        source: pass.installed_agent.source,
        observed_at_ms: pass.installed_agent.observed_at_ms,
    };
    let mut session = SessionFacts {
        uid: io.target().paths().uid,
        os: pass.os.family,
        architecture: pass.architecture,
        hyprland_version: field(pass.hyprland, |f| f.version),
        protocols: field(pass.registry, |f| f.protocols),
        uwsm_managed: field(pass.manager.clone(), |f| f.uwsm_managed),
        graphical_target_active: field(pass.manager, |f| f.graphical_target_active),
        graphical_sessions: field(pass.logind.clone(), |f| f.graphical_sessions),
        selected_session: field(pass.logind, |f| f.selected_session),
        selected_environment,
        manager_environment: pass.manager_environment,
    };
    if let Err(error) = correlation {
        session.protocols.value = Err(error);
    }
    let mut eligibility = classify(&session, &runtime);
    let environment = &session.selected_environment;
    let admission = (|| {
        session_authority(&session)?;
        correlation?;
        if !admitted_pass || environment.runtime_dir != io.target().paths().runtime_home {
            return Err(ProbeIssue::Foreign);
        }
        deadline.check().map_err(issue)?;
        let selected = session
            .selected_session
            .value
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .ok_or(ProbeIssue::Unverified)?;
        let architecture = match session.architecture.value {
            Ok(Architecture::X86_64) => "x86_64",
            Ok(Architecture::Aarch64) => "aarch64",
            _ => "",
        };
        let chosen = &selected.session;
        if environment.wayland_display.is_empty()
            || environment.hyprland_instance_signature.is_empty()
        {
            return Err(ProbeIssue::Unverified);
        }
        let facts = SupportObservations {
            uid: session.uid,
            architecture: architecture.into(),
            arch_based: session.os.value == Ok(OsFamily::Arch),
            hyprland_version: session.hyprland_version.value.unwrap_or_default(),
            protocols_ready: session.protocols.value == Ok(true),
            runtime_libraries_ready: runtime.dependency_graph.value == Ok(true)
                && runtime.libraries.iter().any(|v| v.required)
                && runtime
                    .libraries
                    .iter()
                    .filter(|v| v.required)
                    .all(|v| v.resolved.value.is_ok())
                && [
                    &runtime.ffmpeg,
                    &runtime.opus,
                    &runtime.pipewire_library,
                    &runtime.xkb,
                    &runtime.wayland_library,
                    &runtime.software_video,
                ]
                .iter()
                .all(|fact| fact.value == Ok(true)),
            uwsm_managed: session.uwsm_managed.value?,
            graphical_target_active: session.graphical_target_active.value?,
            graphical_sessions: session.graphical_sessions.value?,
            session_id: chosen.id.clone(),
            session_type: chosen.kind.clone().ok_or(ProbeIssue::Unverified)?,
            seat: chosen.seat.clone().ok_or(ProbeIssue::Unverified)?,
            active: chosen.active == Some(true),
        };
        let proof = SupportProof::admit(io, facts)
            .map_err(issue)?
            .with_advisory(compatibility_report(&session, &runtime));
        deadline.check().map_err(issue)?;
        Ok(Some(proof))
    })();
    let proof = match admission {
        Ok(proof) => proof,
        Err(error) => {
            if !matches!(eligibility, Eligibility::NotSupported(_)) {
                eligibility = Eligibility::Pending(error);
            }
            None
        }
    };
    DetectionResult {
        report: SupportReport {
            eligibility,
            session,
            runtime,
            installed_agent: pass.installed_agent,
            reduced_motion: pass.reduced_motion,
        },
        os_path: pass.os.path,
        backends,
        proof,
    }
}
