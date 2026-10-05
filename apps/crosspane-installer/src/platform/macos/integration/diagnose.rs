//! Read-only Mac detection, including builds without an approved embedded inventory.
use super::super::native_io::{
    Cancellation, CommandSpec, Deadline, LaunchctlAction, MacNativeIo, MacTarget, NativeOperation,
    TargetPaths,
};
use super::*;
use crate::diagnose::{
    Class::{E, R, S},
    Report,
};

pub(crate) fn diagnose(payload: Option<&std::path::Path>, out: &mut Report) {
    out.record(
        "support",
        "system font",
        E,
        discover_system_font().map(|font| font.candidate.path().to_owned()),
        false,
    );
    let clock = monotonic_clock();
    let native_clock: Arc<dyn super::super::native_io::Clock> = Arc::new(FnClock(clock.clone()));
    let Ok(deadline) = Deadline::new(8_000, native_clock.clone(), Cancellation::default()) else {
        out.unavailable_target();
        return;
    };
    let home = PathBuf::from(objc2_foundation::NSHomeDirectory().to_string());
    let root = payload.map(std::path::Path::to_owned).or_else(|| {
        std::env::current_exe().ok().and_then(|exe| {
            exe.parent()
                .and_then(|p| p.parent())
                .filter(|p| p.file_name().is_some_and(|n| n == "Contents"))
                .map(|p| p.join("Resources/payload"))
        })
    });
    let (Some(payload_root), Some(gui_tmpdir)) = (root, probes::gui_tmpdir()) else {
        out.unavailable_target();
        return;
    };
    let target = MacTarget::selected(TargetPaths {
        uid: rustix::process::geteuid().as_raw(),
        home,
        gui_tmpdir,
        runtime_override: None,
        payload_root,
    });
    let Ok(target) = target else {
        out.unavailable_target();
        return;
    };
    let probes = MacProbes::with_system_runner(
        Arc::new(probes::SessionSupport),
        Arc::new(probes::SecuritySignatures),
    );
    let io = MacNativeIo::new(
        target.clone(),
        probes.runner.clone(),
        probes.support.clone(),
        probes.signatures.clone(),
        native_clock.clone(),
    )
    .map(Arc::new);
    let Ok(io) = io else {
        out.unavailable_target();
        return;
    };
    match io.support_observation(&deadline) {
        Ok(observed) => {
            out.record(
                "support",
                "macOS major",
                E,
                Ok::<_, &str>(observed.macos_major),
                false,
            );
            out.record(
                "support",
                "Apple Silicon",
                E,
                Ok::<_, &str>(observed.apple_silicon),
                false,
            );
            if observed.macos_major < 26 || !observed.apple_silicon {
                out.issue(
                    "service",
                    "observed compatibility refusal",
                    E,
                    "Crosspane needs macOS 26 or newer on Apple Silicon",
                    true,
                );
            }
            out.record(
                "support",
                "console UID",
                S,
                Ok::<_, &str>(observed.gui.console_uid),
                false,
            );
            out.record(
                "support",
                "interactive UID",
                S,
                Ok::<_, &str>(observed.gui.interactive_uid),
                false,
            );
            out.record(
                "support",
                "session match",
                S,
                Ok::<_, &str>(observed.gui.console_session == observed.gui.interactive_session),
                false,
            );
            out.record(
                "support",
                "active GUI",
                S,
                Ok::<_, &str>(observed.gui.active),
                false,
            );
            out.record(
                "support",
                "temporary root matches",
                S,
                Ok::<_, &str>(observed.gui_tmpdir == io.target().paths().gui_tmpdir),
                false,
            );
        }
        Err(error) => out.record::<bool, _>("support", "selected GUI session", S, Err(error), true),
    }
    out.record(
        "payload",
        "dead runtime recovery",
        R,
        io.dead_runtime(&deadline).map(|state| state.is_some()),
        false,
    );
    let plist = io
        .target()
        .paths()
        .home
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", super::super::native_io::AGENT_LABEL));
    let program = io.target().agent_path();
    let selected = super::super::launchd_observation::SelectedJob {
        uid: io.target().paths().uid,
        label: super::super::native_io::AGENT_LABEL,
        plist: &plist,
        program: &program,
    };
    let job = CommandSpec::new(
        io.target(),
        NativeOperation::Launchctl(LaunchctlAction::Print),
    )
    .and_then(|spec| io.execute(&spec, None, &deadline))
    .map(|output| {
        super::super::launchd_observation::job(
            output.code,
            &output.stdout,
            &output.stderr,
            selected,
        )
    });
    let job = job.and_then(|job| {
        if job == super::super::launchd_observation::JobObservation::Unknown {
            Err(super::super::native_io::NativeError::Unavailable)
        } else {
            Ok(job)
        }
    });
    out.debug("service", "selected launchd job", S, job, true);
    let disabled = CommandSpec::new(
        io.target(),
        NativeOperation::Launchctl(LaunchctlAction::PrintDisabled),
    )
    .and_then(|spec| io.execute(&spec, None, &deadline))
    .map(|output| {
        super::super::launchd_observation::disabled(
            output.code,
            &output.stdout,
            &output.stderr,
            super::super::native_io::AGENT_LABEL,
        )
    });
    let disabled =
        disabled.and_then(|value| value.ok_or(super::super::native_io::NativeError::Unavailable));
    out.record("service", "selected disabled state", S, disabled, true);
    match embedded_inventory() {
        InventoryAdmission::Present(inventory) => {
            out.record(
                "payload",
                "embedded approved inventory",
                S,
                Ok::<_, &str>(inventory.product_version.clone()),
                false,
            );
            let env = NativeEnv::new(target, *inventory, probes, native_clock);
            admitted(env, out);
        }
        _ => {
            out.issue(
                "payload",
                "embedded approved inventory",
                S,
                "This build has no valid approved inventory",
                true,
            );
            out.issue(
                "payload",
                "owned files and publication",
                S,
                "Approved inventory unavailable",
                true,
            );
            out.issue(
                "agent",
                "matched Status",
                S,
                "Approved inventory unavailable for signed instance admission",
                true,
            );
        }
    }
}

/// Read-only domain observations. In particular, never reserve a durable repair Status watermark.
fn admitted(env: NativeEnv, out: &mut crate::diagnose::Report) {
    use super::super::payload::MacPayload;
    use super::domains::AudioPackages;
    use super::native::{NativeAudio, read_status};
    use crate::agent_contract::{DecodedReply, StatusAdmission};
    use crate::diagnose::Class::{E, R, S};
    use std::sync::atomic::AtomicU64;
    let env = Arc::new(env);
    let deadline = || {
        Deadline::new(
            8_000,
            env.clock.clone(),
            super::super::native_io::Cancellation::default(),
        )
    };
    let Ok(read_deadline) = deadline() else {
        out.unavailable_target();
        return;
    };
    let io = env.io();
    let Ok(io) = io else {
        out.unavailable_target();
        return;
    };
    match MacPayload::admit(io.clone(), env.inventory.clone(), &read_deadline) {
        Ok(payload) => {
            out.record(
                "support",
                "signed session authority",
                S,
                Ok::<_, &str>(true),
                false,
            );
            out.debug(
                "payload",
                "publication recovery",
                R,
                payload.recovery(&read_deadline).map(|r| {
                    (
                        r.record.map(|r| r.phase),
                        r.app_stage_present,
                        r.ctl_stage_present,
                        r.app_previous_present,
                        r.ctl_previous_present,
                        r.retained_temporaries.len(),
                    )
                }),
                false,
            );
            out.debug(
                "payload",
                "owned files",
                S,
                payload
                    .plan(1, 1, None, &read_deadline)
                    .map(|plan| plan.state()),
                true,
            );
        }
        Err(error) => {
            out.record::<bool, _>("payload", "signed payload admission", S, Err(error), true)
        }
    }
    if let Ok(read_deadline) = deadline() {
        match read_status(&env, &AtomicU64::new(1 << 40), &read_deadline) {
            Ok((_, reply)) => match reply.result {
                Ok(DecodedReply::Status(StatusAdmission::Supported(health))) => {
                    let status = health.installer();
                    out.record(
                        "agent",
                        "instance ID",
                        S,
                        Ok::<_, &str>(status.instance.id),
                        false,
                    );
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
                _ => out.issue(
                    "agent",
                    "matched health contract",
                    S,
                    "Status unavailable or unsupported",
                    true,
                ),
            },
            Err(error) => out.record::<bool, _>("agent", "matched Status", S, Err(error), true),
        }
    }
    if let Ok(read_deadline) = deadline() {
        let mut audio = NativeAudio::new(env);
        out.debug(
            "audio",
            "optional driver package",
            E,
            audio.detect(&read_deadline),
            false,
        );
    }
}
