//! Read-only acquisition. Streams come only from the frozen native boundary. No reconnect by
//! pathname, owner-environment fallback, mutation or readiness policy; detect composes support.
mod hyprland;
mod installed;
mod logind;
mod manager;
mod os;
mod registry;
use super::*;
use crate::platform::linux::{native_io::*, transport::CallerClock};
pub use hyprland::{HyprlandFacts, hyprland_from_stream, parse_hyprland_version};
pub use installed::associate_installed;
pub use logind::{LogindFacts, Properties, decode_session, decode_user, logind_from_stream};
pub use manager::{ManagerFacts, UnitRows, decode_units, manager_from_stream};
pub use os::{OsFacts, os_from_reader};
pub use registry::{REQUIRED_PROTOCOLS, RegistryFacts, protocols_satisfy, registry_from_stream};
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
        let source = self.io.target().source();
        let values = self.environment.values();
        let selected_environment = EffectiveEnvironment {
            runtime_dir: self.io.target().paths().runtime_home.clone(),
            wayland_display: values.get("WAYLAND_DISPLAY").cloned().unwrap_or_default(),
            hyprland_instance_signature: values
                .get("HYPRLAND_INSTANCE_SIGNATURE")
                .cloned()
                .unwrap_or_default(),
            session_id: values.get("XDG_SESSION_ID").cloned(),
        };
        let pass = DetectionPass {
            os: self.os(deadline),
            architecture: Fact {
                value: parse_architecture(std::env::consts::ARCH),
                source,
                observed_at_ms: (self.clock)(),
            },
            logind: self.logind(deadline),
            manager: self.manager(deadline),
            manager_environment: self.manager_environment(deadline),
            hyprland: self.hyprland(deadline),
            registry: self.registry(deadline),
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
    pub fn manager(&self, deadline: &Deadline) -> Fact<ManagerFacts> {
        let shared = deadline.clone();
        let clock = self.clock.clone();
        let source = self.io.target().source();
        self.read(
            self.io.connect_session_bus(&self.environment, deadline),
            deadline,
            move |stream| {
                let mut facts = manager::read(stream, &shared, clock)?;
                facts.uwsm_managed.source = source;
                facts.graphical_target_active.source = source;
                Ok(facts)
            },
        )
    }
    pub fn manager_environment(&self, deadline: &Deadline) -> Fact<EffectiveEnvironment> {
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
        manager::environment_output(result, self.io.target().source(), (self.clock)())
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
        let clock = self.clock.clone();
        let source = self.io.target().source();
        self.read(
            self.io.connect_wayland(&self.environment, deadline),
            deadline,
            move |stream| {
                let mut facts = registry::read(stream, clock)?;
                facts.protocols.source = source;
                Ok(facts)
            },
        )
    }
}
