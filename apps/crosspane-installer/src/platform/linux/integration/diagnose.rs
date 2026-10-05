//! The same readers as the GUI, without a worker/controller or any mutating call.
use super::*;
use crate::agent_contract::StatusAdmission;
use crate::diagnose::{
    Class::{E, R, S},
    Report,
};

pub(crate) fn diagnose(payload: Option<&Path>, out: &mut Report) {
    let io = selected_paths()
        .ok()
        .and_then(|paths| LinuxNativeIo::selected(paths).ok())
        .map(Arc::new);
    let Some(io) = io else {
        out.unavailable_target();
        return;
    };
    let env = ChildEnvironment::selected(io.target(), session_env());
    let Ok(env) = env else {
        out.unavailable_target();
        return;
    };
    let clock = monotonic_clock();
    let deadline = || Deadline::new(8_000, Cancellation::default());
    let Ok(read_deadline) = deadline() else {
        out.unavailable_target();
        return;
    };
    out.record(
        "support",
        "system font",
        E,
        detect::fonts::discover(&io, &env, &read_deadline).map(|font| font.path),
        false,
    );
    let package = payload.map(read_package);
    match &package {
        Some(value) => out.record(
            "payload",
            "staged inventory",
            S,
            value.as_ref().map(|p| p.manifest().product_version.clone()),
            true,
        ),
        None => out.issue(
            "payload",
            "staged inventory",
            S,
            "No staged payload selected",
            true,
        ),
    }
    let package = package.and_then(Result::ok);
    let support = NativeSupport {
        io: io.clone(),
        env: env.clone(),
        clock: clock.clone(),
        checks: SupportChecksSlot::new(SUPPORT),
    };
    let Ok(read_deadline) = deadline() else {
        out.unavailable_target();
        return;
    };
    let probes = match detect::NativeSessionProbes::new(io.clone(), env.clone(), clock.clone()) {
        Ok(probes) => probes,
        Err(_) => {
            out.unavailable_target();
            return;
        }
    };
    let result = probes.detect(
        support.runtime_facts(package.as_ref(), &read_deadline),
        &read_deadline,
    );
    let session = &result.report.session;
    let runtime = &result.report.runtime;
    out.debug(
        "support",
        "operating system",
        E,
        session.os.value.as_ref(),
        false,
    );
    out.debug(
        "support",
        "architecture",
        E,
        session.architecture.value.as_ref(),
        false,
    );
    out.record(
        "support",
        "Hyprland version",
        E,
        session.hyprland_version.value,
        false,
    );
    out.record(
        "support",
        "required protocols",
        E,
        session.protocols.value,
        false,
    );
    out.record(
        "support",
        "uwsm lifecycle",
        S,
        session.uwsm_managed.value,
        true,
    );
    out.record(
        "support",
        "graphical target",
        S,
        session.graphical_target_active.value,
        true,
    );
    out.record(
        "support",
        "graphical session count",
        S,
        session.graphical_sessions.value,
        true,
    );
    out.debug(
        "support",
        "selected session",
        S,
        session.selected_session.value.as_ref().map(|s| {
            s.as_ref().map(|s| {
                (
                    &s.session.id,
                    s.session.uid,
                    &s.session.kind,
                    s.session.active,
                    &s.session.seat,
                )
            })
        }),
        true,
    );
    out.record(
        "support",
        "session environment matches",
        S,
        session
            .manager_environment
            .value
            .as_ref()
            .map(|e| e == &session.selected_environment),
        true,
    );
    out.record(
        "support",
        "selected session authority",
        S,
        result
            .proof
            .as_ref()
            .map(|_| true)
            .ok_or("Session identity or admission unknown"),
        true,
    );
    out.debug(
        "support",
        "compatibility",
        E,
        Ok::<_, &str>(&result.report.eligibility),
        false,
    );
    if let detect::Eligibility::NotSupported(reason) = result.report.eligibility {
        let class = match reason {
            detect::UnsupportedReason::Uwsm | detect::UnsupportedReason::SessionType => S,
            _ => E,
        };
        out.issue(
            "service",
            "observed compatibility refusal",
            class,
            &detect::unsupported_text(reason),
            true,
        );
    }
    for (name, value) in [
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
        ("secret-service presence", &runtime.secret_service),
    ] {
        out.record("support", name, E, value.value, false);
    }
    out.debug(
        "agent",
        "keystore provenance",
        S,
        runtime.keystore.value.as_ref(),
        false,
    );
    out.record(
        "support",
        "reduced motion",
        E,
        result.report.reduced_motion.value,
        false,
    );
    for (index, library) in runtime.libraries.iter().enumerate() {
        out.record(
            "support",
            format!("runtime dependency {} ({})", index + 1, library.name),
            E,
            library.resolved.value.as_ref(),
            false,
        );
    }
    out.record(
        "payload",
        "dead runtime recovery",
        R,
        io.dead_runtime().map(|state| state.is_some()),
        false,
    );
    if let (Some(proof), Some(package)) = (&result.proof, &package) {
        let resources = NativePayloads::new(io.clone())
            .and_then(|mut installer| installer.detect(proof, package));
        out.debug("payload", "owned files and publication", S, resources, true);
    } else {
        out.issue(
            "payload",
            "owned files and publication",
            S,
            "Payload or selected session authority unavailable",
            true,
        );
    }
    if let Some(package) = &package {
        let Ok(read_deadline) = deadline() else {
            return;
        };
        let mut service = NativeServices::new(io.clone(), env.clone());
        let prepared = service.prepare(package, &read_deadline);
        out.record(
            "service",
            "manager admission and rendered resources before observation",
            S,
            prepared.map(|()| true),
            true,
        );
        let observed = prepared.and_then(|()| service.observe(&read_deadline));
        match observed {
            Ok(facts) => {
                out.record("service", "enabled", S, Ok::<_, &str>(facts.enabled), false);
                out.record(
                    "service",
                    "active state",
                    S,
                    Ok::<_, &str>(facts.active_state),
                    false,
                );
                out.record(
                    "service",
                    "sub state",
                    S,
                    Ok::<_, &str>(facts.sub_state),
                    false,
                );
                out.record(
                    "service",
                    "main PID",
                    S,
                    Ok::<_, &str>(facts.main_pid),
                    false,
                );
                out.record(
                    "service",
                    "needs reload",
                    S,
                    Ok::<_, &str>(facts.needs_reload),
                    false,
                );
            }
            Err(error) => out.record::<bool, _>(
                "service",
                "selected unit authority and state",
                S,
                Err(error),
                true,
            ),
        }
    } else {
        out.issue(
            "service",
            "selected unit authority and state",
            S,
            "Staged payload unavailable",
            true,
        );
    }
    match &result.report.installed_agent.value {
        Ok(agent) => {
            out.record(
                "agent",
                "instance ID",
                S,
                Ok::<_, &str>(agent.bootstrap.instance_id),
                false,
            );
            out.record("agent", "PID", S, Ok::<_, &str>(agent.bootstrap.pid), false);
            match &agent.status {
                StatusAdmission::Supported(health) => {
                    let status = health.installer();
                    for backend in &status.backends {
                        out.debug(
                            "agent",
                            format!("backend {:?}", backend.name),
                            S,
                            Ok::<_, &str>(&backend.state),
                            false,
                        );
                    }
                    out.debug("agent", "gate", S, Ok::<_, &str>(&status.gate), false);
                    out.debug(
                        "agent",
                        "session",
                        S,
                        Ok::<_, &str>(&status.gate.session),
                        false,
                    );
                    out.record(
                        "agent",
                        "recovery pending",
                        S,
                        Ok::<_, &str>(status.recovery_pending),
                        false,
                    );
                    out.debug(
                        "agent",
                        "startup recovery",
                        S,
                        Ok::<_, &str>(&status.startup_recovery),
                        false,
                    );
                }
                StatusAdmission::PendingHealthContract(_) => out.issue(
                    "agent",
                    "matched health contract",
                    S,
                    "Unsupported health contract",
                    true,
                ),
            }
        }
        Err(issue) => out.record::<bool, _>("agent", "matched Status", S, Err(issue), true),
    }
    out.debug(
        "agent",
        "required backend readiness",
        S,
        result.backends.value.as_ref(),
        true,
    );
    if let Ok(read_deadline) = deadline() {
        let mut firewall = NativeFirewalls::new(io);
        out.debug(
            "network",
            "firewall and link observations",
            E,
            firewall.read(&read_deadline),
            false,
        );
    }
}
