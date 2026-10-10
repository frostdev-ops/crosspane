//! Read-only acquisition. Streams come only from the frozen native boundary. No reconnect by
//! pathname, owner-environment fallback, mutation or readiness policy; detect composes support.
mod hyprland;
mod installed;
mod lineage;
mod logind;
mod manager;
mod os;
mod registry;
mod shell;
use super::*;
use crate::platform::linux::{native_io::*, transport::CallerClock};
pub use hyprland::{HyprlandFacts, hyprland_from_stream, parse_hyprland_version};
pub use installed::associate_installed;
pub use lineage::{
    CompositorLineage, KWIN_WRAPPER, ProcessReader, ProcessStat, START_HYPRLAND,
    compositor_matches, compositor_matches_via, parse_stat, read_lineage,
};
pub use logind::{
    LogindFacts, MAX_PROPERTIES, Properties, decode_session, decode_user, logind_from_stream,
};
pub use manager::{
    GNOME_SHELL, MAX_UNIT_ROWS, ManagerFacts, UnitRows, decode_units, manager_from_stream,
    manager_from_stream_for,
};
pub use os::{OsFacts, os_from_reader};
pub use registry::{
    MAX_REGISTRY_GLOBALS, REQUIRED_PORTAL_PROTOCOLS, REQUIRED_PROTOCOLS, RegistryFacts,
    protocols_satisfy, protocols_satisfy_for, registry_from_stream, registry_from_stream_for,
    required_protocols,
};
pub use shell::{parse_shell_version, shell_version_from_stream};
use std::{
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

static WORKERS: AtomicUsize = AtomicUsize::new(0);
struct Slot;
impl Drop for Slot {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(super) fn issue(error: NativeError) -> ProbeIssue {
    match error {
        NativeError::Timeout => ProbeIssue::Timeout,
        NativeError::Cancelled => ProbeIssue::Cancelled,
        NativeError::Oversize => ProbeIssue::Oversize,
        NativeError::Foreign => ProbeIssue::Foreign,
        NativeError::Invalid => ProbeIssue::Malformed,
        _ => ProbeIssue::Unavailable,
    }
}
/// Four owned workers maximum. Shutdown of our descriptor interrupts authentication
/// as well as method reads. One caller deadline covers the entire exchange; no worker is joined
/// on timeout. Its slot remains occupied until it actually exits, including unwinding.
fn bounded<T: Send + 'static>(
    stream: UnixStream,
    deadline: &Deadline,
    clock: CallerClock,
    source: ObservationSource,
    work: impl FnOnce(UnixStream) -> Result<T, ProbeIssue> + Send + 'static,
) -> Fact<T> {
    let result = (|| {
        deadline.check().map_err(issue)?;
        WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| ProbeIssue::Unavailable)?;
        let slot = Slot;
        let stop = stream.try_clone().map_err(|_| ProbeIssue::Unavailable)?;
        let (send, receive) = mpsc::sync_channel(1);
        let stamp = clock.clone();
        std::thread::Builder::new()
            .name("installer-session-read".into())
            .spawn(move || {
                let _slot = slot;
                let value = work(stream);
                let _ = send.send(Fact {
                    value,
                    source,
                    observed_at_ms: stamp(),
                });
            })
            .map_err(|_| ProbeIssue::Unavailable)?;
        loop {
            if let Err(error) = deadline.check() {
                let _ = stop.shutdown(Shutdown::Both);
                return Err(issue(error));
            }
            match receive.recv_timeout(Duration::from_millis(5)) {
                Ok(fact) => {
                    if let Err(error) = deadline.check() {
                        let _ = stop.shutdown(Shutdown::Both);
                        return Err(issue(error));
                    }
                    return Ok(fact);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return Err(ProbeIssue::Unavailable),
            }
        }
    })();
    result.unwrap_or_else(|error| Fact::issue(error, source, clock()))
}

/// Data-only probes of the selected native target. No support or readiness authority is issued.
pub struct NativeSessionProbes {
    io: Arc<LinuxNativeIo>,
    environment: ChildEnvironment,
    clock: CallerClock,
}
impl std::fmt::Debug for NativeSessionProbes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeSessionProbes")
    }
}
impl NativeSessionProbes {
    pub fn os(&self, deadline: &Deadline) -> OsFacts {
        os_from_reader(
            |request, deadline| self.io.read_system(request, deadline),
            deadline,
            self.io.target().source(),
            &*self.clock,
        )
    }
    pub fn installed_agent(&self, deadline: &Deadline) -> Fact<InstalledAgentFacts> {
        installed::read(self.io.clone(), deadline, self.clock.clone())
    }
    /// One pass and shared deadline, with no cache, mutation, fallback target or preference probe.
    pub fn detect(&self, runtime: RuntimeFacts, deadline: &Deadline) -> DetectionResult {
        let values = self.environment.values();
        let selected_environment = EffectiveEnvironment {
            runtime_dir: self.io.target().paths().runtime_home.clone(),
            wayland_display: values.get("WAYLAND_DISPLAY").cloned().unwrap_or_default(),
            hyprland_instance_signature: values
                .get("HYPRLAND_INSTANCE_SIGNATURE")
                .cloned()
                .unwrap_or_default(),
            session_id: values.get("XDG_SESSION_ID").cloned(),
            xdg_current_desktop: values.get("XDG_CURRENT_DESKTOP").cloned(),
            xdg_session_type: values.get("XDG_SESSION_TYPE").cloned(),
        };
        // The desktop is the agent's own answer for this environment; each desktop has its own
        // evidence chain, and none borrows another's.
        match selected_environment.desktop() {
            Ok(Desktop::Hyprland) => self.detect_hyprland(selected_environment, runtime, deadline),
            Ok(desktop) => self.detect_portal(desktop, selected_environment, runtime, deadline),
            Err(reason) => self.detect_unsupported(reason, selected_environment, runtime, deadline),
        }
    }
    /// An established negative needs no bus: the agent has no backend for this desktop. Nothing
    /// that could grant authority is read, and the report carries no proof.
    fn detect_unsupported(
        &self,
        reason: UnsupportedReason,
        selected_environment: EffectiveEnvironment,
        runtime: RuntimeFacts,
        deadline: &Deadline,
    ) -> DetectionResult {
        let source = self.io.target().source();
        let now = (self.clock)();
        fn unverified<T>(source: ObservationSource, now: u64) -> Fact<T> {
            Fact::issue(ProbeIssue::Unverified, source, now)
        }
        let os = self.os(deadline);
        let session = SessionFacts {
            uid: self.io.target().paths().uid,
            os: os.family,
            architecture: Fact {
                value: parse_architecture(std::env::consts::ARCH),
                source,
                observed_at_ms: now,
            },
            desktop: Err(reason),
            compositor_version: unverified(source, now),
            protocols: unverified(source, now),
            compositor_managed: unverified(source, now),
            graphical_target_active: unverified(source, now),
            graphical_sessions: unverified(source, now),
            selected_session: unverified(source, now),
            selected_environment,
            manager_environment: unverified(source, now),
        };
        let installed_agent = self.installed_agent(deadline);
        DetectionResult {
            backends: super::report::backends_of(&installed_agent, Desktop::Hyprland),
            report: SupportReport {
                eligibility: classify(&session, &runtime),
                session,
                runtime,
                installed_agent,
                reduced_motion: unverified(source, now),
            },
            os_path: os.path,
            proof: None,
        }
    }
    /// GNOME or KDE Plasma. The chain mirrors uwsm's: the user manager's own compositor unit is
    /// the running process that serves this session's Wayland socket, inside the selected
    /// logind session, with the manager's environment agreeing with the installer's.
    fn detect_portal(
        &self,
        desktop: Desktop,
        selected_environment: EffectiveEnvironment,
        runtime: RuntimeFacts,
        deadline: &Deadline,
    ) -> DetectionResult {
        let source = self.io.target().source();
        let architecture = Fact {
            value: parse_architecture(std::env::consts::ARCH),
            source,
            observed_at_ms: (self.clock)(),
        };
        let os = self.os(deadline);
        let logind = self.logind(deadline);
        let manager = self.manager_for(deadline, desktop);
        let manager_environment = self.manager_environment_for(deadline, desktop);
        let registry = self.registry_for(deadline, desktop);
        let lineage = self.lineage_between(
            &manager,
            registry.value.as_ref().ok().map(|facts| facts.pid),
            desktop,
            deadline,
        );
        let version = match desktop {
            Desktop::Gnome => self.shell_version(deadline),
            _ => Fact::issue(ProbeIssue::Unverified, source, (self.clock)()),
        };
        let pass = PortalPass {
            desktop,
            os,
            architecture,
            logind,
            manager,
            manager_environment,
            registry,
            lineage,
            version,
            installed_agent: self.installed_agent(deadline),
            reduced_motion: Fact::issue(ProbeIssue::Unverified, source, (self.clock)()),
        };
        super::report::compose_portal_support(
            &self.io,
            selected_environment,
            pass,
            runtime,
            deadline,
            true,
        )
    }
    fn detect_hyprland(
        &self,
        selected_environment: EffectiveEnvironment,
        runtime: RuntimeFacts,
        deadline: &Deadline,
    ) -> DetectionResult {
        let source = self.io.target().source();
        let os = self.os(deadline);
        let architecture = Fact {
            value: parse_architecture(std::env::consts::ARCH),
            source,
            observed_at_ms: (self.clock)(),
        };
        let logind = self.logind(deadline);
        let manager = self.manager(deadline);
        let manager_environment = self.manager_environment(deadline);
        let hyprland = self.hyprland(deadline);
        let registry = self.registry(deadline);
        let lineage = self.lineage(&manager, &hyprland, deadline);
        let pass = DetectionPass {
            os,
            architecture,
            logind,
            manager,
            manager_environment,
            hyprland,
            registry,
            lineage,
            installed_agent: self.installed_agent(deadline),
            reduced_motion: Fact::issue(ProbeIssue::Unverified, source, (self.clock)()),
        };
        super::report::compose_support(
            &self.io,
            selected_environment,
            pass,
            runtime,
            deadline,
            true,
        )
    }
    pub fn new(
        io: Arc<LinuxNativeIo>,
        environment: ChildEnvironment,
        clock: CallerClock,
    ) -> Result<Self, NativeError> {
        io.validate_target()?;
        Ok(Self {
            io,
            environment,
            clock,
        })
    }
    fn read<T: Send + 'static>(
        &self,
        stream: Result<UnixStream, NativeError>,
        deadline: &Deadline,
        work: impl FnOnce(UnixStream) -> Result<T, ProbeIssue> + Send + 'static,
    ) -> Fact<T> {
        match stream {
            Ok(stream) => bounded(
                stream,
                deadline,
                self.clock.clone(),
                self.io.target().source(),
                work,
            ),
            Err(error) => Fact::issue(issue(error), self.io.target().source(), (self.clock)()),
        }
    }
    pub fn logind(&self, deadline: &Deadline) -> Fact<LogindFacts> {
        let uid = self.io.target().paths().uid;
        let id = self.environment.values().get("XDG_SESSION_ID").cloned();
        let clock = self.clock.clone();
        let shared = deadline.clone();
        let source = self.io.target().source();
        self.read(
            self.io.connect_system_bus(deadline),
            deadline,
            move |stream| {
                let mut facts = logind::read(stream, uid, std::process::id(), id, &shared, &clock)?;
                facts.selected_session.source = source;
                facts.graphical_sessions.source = source;
                Ok(facts)
            },
        )
    }
    /// /proc lineage only when uwsm's MainPID isn't itself the Hyprland IPC peer. Scratch targets
    /// never read the host's /proc.
    pub fn lineage(
        &self,
        manager: &Fact<ManagerFacts>,
        hyprland: &Fact<HyprlandFacts>,
        deadline: &Deadline,
    ) -> Fact<CompositorLineage> {
        self.lineage_between(
            manager,
            hyprland.value.as_ref().ok().map(|facts| facts.pid),
            Desktop::Hyprland,
            deadline,
        )
    }
    /// The same read for any desktop, with the compositor PID taken from its socket peer. GNOME
    /// has no launcher: its Shell must be the unit's main process itself, so nothing is read.
    pub fn lineage_between(
        &self,
        manager: &Fact<ManagerFacts>,
        compositor: Option<u32>,
        desktop: Desktop,
        deadline: &Deadline,
    ) -> Fact<CompositorLineage> {
        let source = self.io.target().source();
        let value = match (&manager.value, compositor) {
            (Ok(manager), Some(compositor)) => match manager.compositor_pid {
                Some(main) if main != compositor && desktop != Desktop::Gnome => {
                    if source == ObservationSource::Demo {
                        Err(ProbeIssue::Foreign)
                    } else {
                        deadline
                            .check()
                            .map_err(issue)
                            .and_then(|_| read_lineage(&lineage::NativeProcesses, main, compositor))
                    }
                }
                _ => Err(ProbeIssue::Unverified),
            },
            _ => Err(ProbeIssue::Unverified),
        };
        Fact {
            value,
            source,
            observed_at_ms: (self.clock)(),
        }
    }
    pub fn manager(&self, deadline: &Deadline) -> Fact<ManagerFacts> {
        self.manager_for(deadline, Desktop::Hyprland)
    }
    pub fn manager_for(&self, deadline: &Deadline, desktop: Desktop) -> Fact<ManagerFacts> {
        let shared = deadline.clone();
        let clock = self.clock.clone();
        let source = self.io.target().source();
        self.read(
            self.io.connect_session_bus(&self.environment, deadline),
            deadline,
            move |stream| {
                let mut facts = manager::read_for(stream, &shared, clock, desktop)?;
                facts.compositor_managed.source = source;
                facts.graphical_target_active.source = source;
                Ok(facts)
            },
        )
    }
    /// GNOME Shell's version (advisory; never authority).
    pub fn shell_version(&self, deadline: &Deadline) -> Fact<[u16; 3]> {
        let shared = deadline.clone();
        let clock = self.clock.clone();
        self.read(
            self.io.connect_session_bus(&self.environment, deadline),
            deadline,
            move |stream| shell::read(stream, &shared, clock),
        )
    }
    pub fn manager_environment(&self, deadline: &Deadline) -> Fact<EffectiveEnvironment> {
        self.manager_environment_for(deadline, Desktop::Hyprland)
    }
    pub fn manager_environment_for(
        &self,
        deadline: &Deadline,
        desktop: Desktop,
    ) -> Fact<EffectiveEnvironment> {
        let result = (|| {
            let bus = self
                .environment
                .values()
                .get("DBUS_SESSION_BUS_ADDRESS")
                .map(|v| {
                    std::collections::BTreeMap::from([(
                        "DBUS_SESSION_BUS_ADDRESS".into(),
                        v.clone(),
                    )])
                })
                .unwrap_or_default();
            let environment = self.io.manager_environment(bus, deadline)?;
            let command = CommandSpec::new(
                "/usr/bin/systemctl".into(),
                vec!["--user".into(), "show-environment".into()],
                environment,
                MAX_PROBE_BYTES,
            )?;
            self.io.run(&command, deadline)
        })();
        manager::environment_output(result, self.io.target().source(), (self.clock)(), desktop)
    }
    pub fn hyprland(&self, deadline: &Deadline) -> Fact<HyprlandFacts> {
        let clock = self.clock.clone();
        let source = self.io.target().source();
        self.read(
            self.io.connect_hyprland(&self.environment, deadline),
            deadline,
            move |stream| {
                let mut facts = hyprland::read(stream, clock)?;
                facts.version.source = source;
                Ok(facts)
            },
        )
    }
    pub fn registry(&self, deadline: &Deadline) -> Fact<RegistryFacts> {
        self.registry_for(deadline, Desktop::Hyprland)
    }
    pub fn registry_for(&self, deadline: &Deadline, desktop: Desktop) -> Fact<RegistryFacts> {
        let clock = self.clock.clone();
        let source = self.io.target().source();
        self.read(
            self.io.connect_wayland(&self.environment, deadline),
            deadline,
            move |stream| {
                let mut facts = registry::read_for(stream, clock, desktop)?;
                facts.protocols.source = source;
                Ok(facts)
            },
        )
    }
}
