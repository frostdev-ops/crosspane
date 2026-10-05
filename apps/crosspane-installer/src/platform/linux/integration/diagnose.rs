//! The same readers as the GUI, without a worker/controller or any mutating call.
use super::super::{
    native_io::{CommandSpec, MANAGER_PROPERTIES},
    service::{ServiceError, UNIT},
};
use super::*;
use crate::agent_contract::StatusAdmission;
use crate::diagnose::{
    Class::{E, R, S},
    Report,
};

/// Fresh absence is its own diagnostic state, never an installed-service or readiness proof.
fn service_absent(
    io: &LinuxNativeIo,
    env: &ChildEnvironment,
    deadline: &Deadline,
) -> std::result::Result<(), NativeError> {
    let paths = io.target().paths();
    let resources = [
        paths.config_home.join("systemd/user").join(UNIT),
        paths
            .data_home
            .join("applications/crosspane-settings.desktop"),
        paths
            .data_home
            .join("applications/crosspane-installer.desktop"),
        paths
            .config_home
            .join("systemd/user/graphical-session.target.wants")
            .join(UNIT),
    ];
    let absent = || {
        for path in &resources {
            deadline.check()?;
            if !io.path_is_absent(path)? {
                return Err(NativeError::Foreign);
            }
        }
        Ok(())
    };
    absent()?;
    // The same selected, pinned manager endpoint and read-only command as LinuxService.
    let environment = io.manager_environment(manager_session_of(env.values()), deadline)?;
    let command = CommandSpec::new(
        "/usr/bin/systemctl".into(),
        vec![
            "--user".into(),
            "show".into(),
            "--all".into(),
            UNIT.into(),
            "-p".into(),
            MANAGER_PROPERTIES.into(),
        ],
        environment,
        detect::MAX_PROBE_BYTES,
    )?;
    let output = io.run(&command, deadline)?;
    if output.code != Some(0) || !output.stderr.is_empty() {
        return Err(NativeError::Unavailable);
    }
    let expected = BTreeMap::from([
        ("Id", UNIT),
        ("LoadState", "not-found"),
        ("ActiveState", "inactive"),
        ("SubState", "dead"),
        ("FragmentPath", ""),
        ("DropInPaths", ""),
        ("UnitFileState", ""),
        ("MainPID", "0"),
    ]);
    let text = std::str::from_utf8(&output.stdout).map_err(|_| NativeError::Invalid)?;
    let mut seen = std::collections::BTreeSet::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            if expected
                .keys()
                .any(|key| line == *key || line.starts_with(&format!("{key} ")))
            {
                return Err(NativeError::Invalid);
            }
            continue;
        };
        if let Some(required) = expected.get(key)
            && (value != *required || !seen.insert(key))
        {
            return Err(NativeError::Foreign);
        }
    }
    if seen.len() != expected.len() {
        return Err(NativeError::Unavailable);
    }
    // Do not turn a partial install appearing during the manager query into fresh absence.
    absent()?;
    deadline.check()
}

fn record_service_error(
    io: &LinuxNativeIo,
    env: &ChildEnvironment,
    prepared: bool,
    error: ServiceError,
    deadline: &Deadline,
    out: &mut Report,
) {
    let check = "selected unit authority and state";
    if prepared
        && error == ServiceError::Native(NativeError::Foreign)
        && service_absent(io, env, deadline).is_ok()
    {
        out.record(
            "service",
            check,
            S,
            Ok::<_, NativeError>("Not installed yet; setup will install Crosspane's startup entry"),
            false,
        );
    } else {
        out.record::<bool, _>("service", check, S, Err(error), true);
    }
}

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
            Err(error) => {
                record_service_error(&io, &env, prepared.is_ok(), error, &read_deadline, out)
            }
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::super::native_io::{
        CommandOutput, CommandRunner, ProcessFacts, ProcessProbe,
    };
    use super::*;
    use std::{
        fs,
        os::unix::{
            fs::{DirBuilderExt, PermissionsExt, symlink},
            net::UnixListener,
        },
        sync::atomic::{AtomicUsize, Ordering},
    };

    // Captured by the lead on the clean systemd 261 desktop, including order and final LF.
    const NOT_FOUND: &[u8] = b"Id=crosspane-agent.service\nLoadState=not-found\nActiveState=inactive\nSubState=dead\nFragmentPath=\nSourcePath=\nDropInPaths=\nUnitFileState=\nMainPID=0\n";
    type Hook = Box<dyn FnOnce() + Send>;
    struct Manager {
        output: Mutex<CommandOutput>,
        hook: Mutex<Option<Hook>>,
        calls: AtomicUsize,
    }
    impl CommandRunner for Manager {
        fn run(
            &self,
            command: &CommandSpec,
            deadline: &Deadline,
        ) -> std::result::Result<CommandOutput, NativeError> {
            deadline.check()?;
            assert_eq!(command.executable(), Path::new("/usr/bin/systemctl"));
            assert_eq!(
                command.argv(),
                ["--user", "show", "--all", UNIT, "-p", MANAGER_PROPERTIES]
            );
            self.calls.fetch_add(1, Ordering::Relaxed);
            if let Some(hook) = self.hook.lock().unwrap().take() {
                hook();
            }
            let output = self.output.lock().unwrap();
            Ok(CommandOutput {
                code: output.code,
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
            })
        }
    }
    impl ProcessProbe for Manager {
        fn snapshot(&self, _: u32, _: &Deadline) -> std::result::Result<ProcessFacts, NativeError> {
            panic!("diagnosing service absence must not inspect or stop a process");
        }
    }
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn private_dir(path: &Path) {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .unwrap();
    }

    #[test]
    fn fresh_service_diagnosis_requires_exact_absence_and_admitted_not_found() {
        let root = Scratch(PathBuf::from(format!(
            "/tmp/crosspane-diagnose-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )));
        let manager = Arc::new(Manager {
            output: Mutex::new(CommandOutput {
                code: Some(0),
                stdout: NOT_FOUND.to_vec(),
                stderr: Vec::new(),
            }),
            hook: Mutex::new(None),
            calls: AtomicUsize::new(0),
        });
        let io = LinuxNativeIo::scratch(&root.0, manager.clone(), manager.clone()).unwrap();
        let paths = io.target().paths();
        // Same remaining configuration directory as the owner's cleaned machine.
        private_dir(&paths.config_home.join("crosspane"));
        private_dir(&paths.runtime_home.join("systemd"));
        let socket_path = paths.runtime_home.join("systemd/private");
        let _socket = UnixListener::bind(&socket_path).unwrap();
        let env = ChildEnvironment::selected(io.target(), BTreeMap::new()).unwrap();
        let unit = paths.config_home.join("systemd/user").join(UNIT);
        let settings = paths
            .data_home
            .join("applications/crosspane-settings.desktop");
        let installer = paths
            .data_home
            .join("applications/crosspane-installer.desktop");
        let link = paths
            .config_home
            .join("systemd/user/graphical-session.target.wants")
            .join(UNIT);
        let foreign = ServiceError::Native(NativeError::Foreign);
        let fact = |error, prepared| {
            let mut report = Report::new();
            record_service_error(
                &io,
                &env,
                prepared,
                error,
                &Deadline::new(5000, Cancellation::default()).unwrap(),
                &mut report,
            );
            let report = serde_json::to_value(report).unwrap();
            assert_eq!(report["facts"].as_array().unwrap().len(), 1);
            let fact = report["facts"][0].clone();
            assert_eq!(fact["class"], "S");
            assert_eq!(fact["check"], "selected unit authority and state");
            fact
        };
        let blocked = || {
            let fact = fact(foreign, true);
            assert_eq!(fact["hard_stop"], true);
            assert_eq!(fact["issue"], "Native(Foreign)");
        };
        // Installed-resource reads retain their strict ENOENT behavior.
        assert_eq!(
            io.read(&unit, 256, false).unwrap_err(),
            NativeError::Foreign
        );
        let absent = fact(foreign, true);
        assert_eq!(absent["hard_stop"], false);
        assert_eq!(absent["issue"], serde_json::Value::Null);
        assert_eq!(
            absent["value"],
            "Not installed yet; setup will install Crosspane's startup entry"
        );
        for path in [&unit, &settings, &installer, &link] {
            private_dir(path.parent().unwrap());
            fs::write(path, b"partial install").unwrap();
            let calls = manager.calls.load(Ordering::Relaxed);
            blocked();
            assert_eq!(manager.calls.load(Ordering::Relaxed), calls);
            fs::remove_file(path).unwrap();
        }
        // A dangling enable link is present, not a proof of absence.
        symlink(&unit, &link).unwrap();
        blocked();
        fs::remove_file(&link).unwrap();
        // Unsafe ancestors, incomplete/malformed/duplicate facts, stderr and errors refuse.
        fs::set_permissions(
            settings.parent().unwrap(),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        blocked();
        fs::set_permissions(
            settings.parent().unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        for stdout in [
            String::from_utf8_lossy(NOT_FOUND)
                .replace("not-found", "loaded")
                .into_bytes(),
            String::from_utf8_lossy(NOT_FOUND)
                .replace("MainPID=0", "MainPID=42")
                .into_bytes(),
            String::from_utf8_lossy(NOT_FOUND)
                .replace("FragmentPath=", "FragmentPath=/foreign/unit")
                .into_bytes(),
            String::from_utf8_lossy(NOT_FOUND)
                .replace("DropInPaths=", "DropInPaths=/foreign/drop-in")
                .into_bytes(),
            String::from_utf8_lossy(NOT_FOUND)
                .replace("UnitFileState=", "UnitFileState=enabled")
                .into_bytes(),
            String::from_utf8_lossy(NOT_FOUND)
                .replace("SubState=dead\n", "")
                .into_bytes(),
            [NOT_FOUND, b"MainPID=0\n"].concat(),
            [NOT_FOUND, b"MainPID malformed\n"].concat(),
            vec![0xff],
        ] {
            manager.output.lock().unwrap().stdout = stdout;
            blocked();
        }
        manager.output.lock().unwrap().stdout = NOT_FOUND.to_vec();
        manager.output.lock().unwrap().code = Some(1);
        blocked();
        manager.output.lock().unwrap().code = Some(0);
        manager.output.lock().unwrap().stderr = b"unavailable".to_vec();
        blocked();
        manager.output.lock().unwrap().stderr.clear();
        assert_eq!(fact(foreign, false)["hard_stop"], true);
        assert_eq!(fact(ServiceError::Unknown, true)["hard_stop"], true);
        assert_eq!(fact(foreign, true)["hard_stop"], false); // existing safe, empty parents too
        let appeared = unit.clone();
        *manager.hook.lock().unwrap() = Some(Box::new(move || {
            fs::write(appeared, b"appeared during query").unwrap()
        }));
        blocked();
        fs::remove_file(&unit).unwrap();
        // The native runner still revalidates the same manager socket after the query.
        *manager.hook.lock().unwrap() = Some(Box::new(move || {
            fs::rename(&socket_path, socket_path.with_extension("old")).unwrap();
            let _replacement = UnixListener::bind(socket_path).unwrap();
        }));
        blocked();
    }
}
