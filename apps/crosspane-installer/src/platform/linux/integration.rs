//! The Linux production binding: native adapters on one worker thread, the agent port and the
//! practice fixture on the GUI thread, composed behind the shared `live::Platform` port.
//!
//! Nothing here decides what a result means. `worker.rs` holds that policy; `domains.rs` wraps
//! each merged native module; `ports.rs` carries the GUI-thread ports, which ask the worker for a
//! fresh support proof (they live five seconds) right before every mutation.
//!
//! There is no fake mode in production: `open` composes only the native domains. `compose` takes
//! the same parts from tests, whose domains are in-memory or scratch targets.

/// Type names only: these hold handles and plans whose contents never belong in a log.
macro_rules! opaque_debug {
    ($($ty:ty),+ $(,)?) => {$(
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(stringify!($ty))
            }
        }
    )+};
}

mod domains;
mod ports;
mod repair;
mod resume;
mod uninstall;
mod worker;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use crosspane_installer_core::StepId;
use eframe::egui;

use super::detect;
use super::native_io::{
    Cancellation, ChildEnvironment, Deadline, LinuxNativeIo, NativeError, TargetPaths,
};
use super::payload::{Architecture, Package};
use super::transport::LinuxAgentPort;
use crate::agent_contract::{AgentPlatform, AgentPort};
use crate::gui::InstallerController;
use crate::live::{
    Clock, LiveController, LiveError, NativeJob, NativeRefusal, NativeReport, NativeStep, Platform,
    PlatformDescription, PracticeFixtures,
};
use crate::tutorial_flow::TutorialSourcePolicy;
use crate::view::{ProgressGroup, ScreenId};

pub use domains::{
    DomainFactory, Domains, FirewallReading, Firewalls, NativeFirewalls, NativePayloads,
    NativeServices, NativeSupport, NoRepairer, NoUninstaller, PayloadPreview, Payloads,
    RepairFinish, RepairOffer, RepairStep, Repairer, RuleApply, RulePresence, Services, Support,
    SupportOutcome, UninstallOffer, UninstallProgress, Uninstaller,
};
pub use ports::{AgentSlot, Command, LinuxPractice, ProofBroker, SupportedAgentPort};
pub use repair::NativeRepairer;
pub use uninstall::NativeUninstaller;

pub const SUPPORT: StepId = StepId(10);
pub const PAYLOAD: StepId = StepId(20);
pub const SERVICE: StepId = StepId(21);
pub const RESTART: StepId = StepId(22);
pub const AGENT: StepId = StepId(23);
pub const NETWORK: StepId = StepId(30);

/// Production entry: the system font first, then the live controller. Only `gui::run` calls
/// this, and only for a normal (non-demo) launch.
pub fn open(
    payload: Option<PathBuf>,
) -> Result<(egui::FontDefinitions, Box<dyn InstallerController>)> {
    let clock = monotonic_clock();
    let io = Arc::new(LinuxNativeIo::selected(selected_paths()?).map_err(native)?);
    let env = ChildEnvironment::selected(io.target(), session_env()).map_err(native)?;
    let deadline = Deadline::new(8_000, Cancellation::default()).map_err(native)?;
    let font =
        detect::fonts::discover(&io, &env, &deadline).context("Cannot read a system font")?;
    let platform = LinuxPlatform::production(io, env, payload, font.path.clone(), clock.clone())?;
    let controller = LiveController::new(Box::new(platform), clock).map_err(live_err)?;
    Ok((font.definitions, Box::new(controller)))
}

fn monotonic_clock() -> Clock {
    let start = Instant::now();
    Arc::new(move || start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
}

fn native(error: NativeError) -> anyhow::Error {
    anyhow::anyhow!("{error}")
}

fn live_err(error: LiveError) -> anyhow::Error {
    anyhow::anyhow!("{error}")
}

fn selected_paths() -> Result<TargetPaths> {
    let uid = rustix::process::geteuid().as_raw();
    if uid == 0 {
        bail!("The installer refuses to run as root.");
    }
    let home = PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
    if !home.starts_with("/home") {
        bail!("This account's home directory is not a supported ordinary-user path.");
    }
    let or_home = |key: &str, rel: &str| {
        std::env::var_os(key)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(rel))
    };
    Ok(TargetPaths {
        uid,
        home: home.clone(),
        prefix: home.join(".local"),
        config_home: or_home("XDG_CONFIG_HOME", ".config"),
        state_home: or_home("XDG_STATE_HOME", ".local/state"),
        data_home: or_home("XDG_DATA_HOME", ".local/share"),
        runtime_home: PathBuf::from(format!("/run/user/{uid}")),
        runtime_override: None,
    })
}

fn session_env() -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for key in [
        "DBUS_SESSION_BUS_ADDRESS",
        "WAYLAND_DISPLAY",
        "HYPRLAND_INSTANCE_SIGNATURE",
        "XDG_SESSION_ID",
        "XDG_SESSION_TYPE",
    ] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            values.insert(key.into(), value);
        }
    }
    values
}

/// The session facts the manager adapters accept: only the five variables the environment was
/// built from, never the full child environment (which also carries PATH, HOME and XDG paths).
fn session_of(env: &ChildEnvironment) -> BTreeMap<String, String> {
    env.values()
        .iter()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "DBUS_SESSION_BUS_ADDRESS"
                    | "WAYLAND_DISPLAY"
                    | "HYPRLAND_INSTANCE_SIGNATURE"
                    | "XDG_SESSION_ID"
                    | "XDG_SESSION_TYPE"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn machine_label() -> String {
    let name = rustix::system::uname();
    let node = name.nodename().to_string_lossy();
    if node.is_empty() || node == "(none)" {
        "This computer".into()
    } else {
        node.into()
    }
}

struct StepFlags {
    installed: bool,
    uses_status: bool,
    settles_with_peer: bool,
}

fn step(
    id: StepId,
    prerequisites: &[StepId],
    screen: ScreenId,
    flags: StepFlags,
    label: &str,
    action: &str,
) -> NativeStep {
    NativeStep {
        id,
        prerequisites: prerequisites.to_vec(),
        required_for_installed: flags.installed,
        required_for_ready: true,
        screen,
        group: if screen == ScreenId::Network {
            ProgressGroup::PermissionsNetwork
        } else {
            ProgressGroup::Install
        },
        label: label.into(),
        action_label: action.into(),
        uses_status: flags.uses_status,
        settles_with_peer: flags.settles_with_peer,
        agent_apply: None,
    }
}

/// The Linux readiness graph, in the shared controller's vocabulary.
///
/// Support, files and startup make Crosspane *installed*. The restart, the agent and the network
/// are live checks: the restart and agent steps each need a status issued for them, and the
/// network step is only proved by the other computer actually connecting.
pub fn description(resume_note: Option<String>) -> PlatformDescription {
    let install = |installed| StepFlags {
        installed,
        uses_status: false,
        settles_with_peer: false,
    };
    let live_check = StepFlags {
        installed: false,
        uses_status: true,
        settles_with_peer: false,
    };
    PlatformDescription {
        platform: AgentPlatform::Linux,
        machine_label: machine_label(),
        steps: vec![
            step(
                SUPPORT,
                &[],
                ScreenId::Compatibility,
                install(true),
                "This computer can run Crosspane",
                "Check this computer",
            ),
            step(
                PAYLOAD,
                &[SUPPORT],
                ScreenId::InstallPlan,
                install(true),
                "Crosspane is installed for your account",
                "Install",
            ),
            step(
                SERVICE,
                &[PAYLOAD],
                ScreenId::Installing,
                install(true),
                "Crosspane starts when you sign in",
                "Set up startup",
            ),
            step(
                RESTART,
                &[SERVICE],
                ScreenId::Installing,
                StepFlags { ..live_check },
                "A restart brings up a new, healthy instance",
                "Restart once",
            ),
            step(
                AGENT,
                &[RESTART],
                ScreenId::Installing,
                StepFlags { ..live_check },
                "Crosspane is running with its key in the system keyring",
                "Check the agent",
            ),
            step(
                NETWORK,
                &[AGENT],
                ScreenId::Network,
                StepFlags {
                    installed: false,
                    uses_status: true,
                    settles_with_peer: true,
                },
                "This computer can reach the other one on your network",
                "Allow Crosspane on the network",
            ),
        ],
        connect_after: vec![AGENT],
        practice_after: vec![AGENT, NETWORK],
        hiding_choice: false,
        source_policy: TutorialSourcePolicy::Native,
        speakers_device: Some("crosspane.{peer}.speaker".into()),
        resume_note,
    }
}

/// The parts of the Linux binding. Production fills these with the native domains; tests supply
/// in-memory or scratch ones.
pub struct Parts {
    pub io: Arc<LinuxNativeIo>,
    pub env: ChildEnvironment,
    pub clock: Clock,
    /// The staged payload directory (`payload.tar` and `payload.sha256`).
    pub payload: Option<PathBuf>,
    pub support: Arc<dyn Support>,
    /// Builds the worker's native domains on the worker thread.
    pub domains: DomainFactory,
    pub agent: AgentSource,
    pub fixtures: FixtureSource,
    /// An already-validated package, for tests that don't stage one on disk.
    #[doc(hidden)]
    pub package: Option<Package>,
}

pub enum AgentSource {
    /// The bounded socket port to the installed agent.
    Native,
    #[doc(hidden)]
    Injected(Box<dyn SupportedAgentPort>),
}

pub enum FixtureSource {
    /// The owned practice window, started once a fresh support proof arrives.
    Native { font: PathBuf },
    #[doc(hidden)]
    Injected(Box<dyn PracticeFixtures>),
}

pub struct LinuxPlatform {
    desc: PlatformDescription,
    commands: Option<mpsc::SyncSender<Command>>,
    reports: Receiver<NativeReport>,
    broker: Arc<ProofBroker>,
    agent: AgentSlot,
    fixtures: Box<dyn PracticeFixtures>,
    stop: Arc<Cancellation>,
}

opaque_debug!(LinuxPlatform, Parts, AgentSource, FixtureSource);

impl LinuxPlatform {
    fn production(
        io: Arc<LinuxNativeIo>,
        env: ChildEnvironment,
        payload: Option<PathBuf>,
        font: PathBuf,
        clock: Clock,
    ) -> Result<Self> {
        let support = Arc::new(NativeSupport {
            io: io.clone(),
            env: env.clone(),
            clock: clock.clone(),
        });
        // Validate the install paths here, so a bad target is a startup error, not a dead worker.
        NativePayloads::new(io.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let native_io = io.clone();
        let native_env = env.clone();
        let native_clock = clock.clone();
        let native_support = support.clone();
        let domains: DomainFactory = Box::new(move || Domains {
            payloads: match NativePayloads::new(native_io.clone()) {
                Ok(payloads) => Box::new(payloads),
                Err(_) => Box::new(domains::BrokenPayloads),
            },
            services: Box::new(NativeServices::new(native_io.clone(), native_env.clone())),
            firewalls: Box::new(NativeFirewalls::new(native_io.clone())),
            uninstaller: Box::new(uninstall::NativeUninstaller::new(
                native_io.clone(),
                native_env.clone(),
                native_clock,
                native_support.clone(),
            )),
            repairer: Box::new(repair::NativeRepairer::new(
                native_io,
                native_env,
                native_support,
            )),
        });
        Self::start(Parts {
            io,
            env,
            clock,
            payload,
            support,
            domains,
            agent: AgentSource::Native,
            fixtures: FixtureSource::Native { font },
            package: None,
        })
    }

    /// Compose from injected parts: the test seam. It exists only in this crate's own tests and
    /// when the non-default `test-hooks` feature is on (the installer's integration tests turn it
    /// on through a dev-dependency on this crate); a production build has no way to inject
    /// domains, an agent port or fixtures.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn compose(parts: Parts) -> Result<Self> {
        Self::start(parts)
    }

    /// Start the worker and wire the GUI-thread ports to it.
    fn start(parts: Parts) -> Result<Self> {
        let resume_note = resume::read(&parts.io).as_ref().and_then(resume::note);
        let (commands, receiver) = mpsc::sync_channel::<Command>(8);
        let (reports_tx, reports_rx) = mpsc::sync_channel::<NativeReport>(64);
        let (broker, results) = ProofBroker::new(commands.clone());
        let tutorial_hash = Arc::new(Mutex::new(None));
        let stop = Arc::new(Cancellation::default());
        let inner: Box<dyn SupportedAgentPort> = match parts.agent {
            AgentSource::Native => Box::new(
                LinuxAgentPort::new(parts.io.clone(), None, parts.clock.clone()).map_err(native)?,
            ),
            AgentSource::Injected(port) => port,
        };
        let fixtures: Box<dyn PracticeFixtures> = match parts.fixtures {
            FixtureSource::Native { font } => Box::new(LinuxPractice::new(
                parts.io.clone(),
                parts.env.clone(),
                parts.clock.clone(),
                font,
                broker.clone(),
                tutorial_hash.clone(),
            )),
            FixtureSource::Injected(fixtures) => fixtures,
        };
        let worker = worker::WorkerParts {
            io: parts.io,
            clock: parts.clock.clone(),
            payload_dir: parts.payload,
            support: parts.support,
            domains: parts.domains,
            reports: reports_tx,
            results,
            tutorial_hash,
            stop: stop.clone(),
            package: parts.package,
        };
        thread::Builder::new()
            .name("installer-linux".into())
            .spawn(move || {
                if let Ok(worker) = worker::Worker::new(worker) {
                    worker.run(receiver);
                }
            })
            .context("Cannot start the Linux installer worker")?;
        Ok(Self {
            desc: description(resume_note),
            commands: Some(commands),
            reports: reports_rx,
            agent: AgentSlot::new(inner, broker.clone(), parts.clock),
            broker,
            fixtures,
            stop,
        })
    }
}

impl Platform for LinuxPlatform {
    fn describe(&self) -> PlatformDescription {
        self.desc.clone()
    }

    fn submit(&mut self, job: NativeJob) -> Result<(), NativeRefusal> {
        let Some(sender) = &self.commands else {
            return Err(NativeRefusal::Busy);
        };
        sender
            .try_send(Command::Job(job))
            .map_err(|_| NativeRefusal::Busy)
    }

    fn poll(&mut self) -> Vec<NativeReport> {
        let mut out = Vec::new();
        while let Ok(report) = self.reports.try_recv() {
            out.push(report);
        }
        out
    }

    fn agent(&mut self) -> &mut dyn AgentPort {
        &mut self.agent
    }

    fn fixtures(&mut self) -> &mut dyn PracticeFixtures {
        self.fixtures.as_mut()
    }

    fn shutdown(&mut self) {
        self.stop.cancel();
        self.broker.close();
        self.commands = None;
        self.fixtures.retire();
    }
}

/// Open one staged file without following a link and without blocking (a FIFO or device in the
/// payload folder can never wedge the worker): only a regular file is accepted.
fn open_staged(path: &Path) -> Option<std::fs::File> {
    use rustix::fs::{FileType, Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let stat = rustix::fs::fstat(&fd).ok()?;
    (FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile)
        .then(|| std::fs::File::from(fd))
}

/// `payload.sha256` holds one hex digest and an optional name; anything longer is not one.
const MAX_SUM_BYTES: u64 = 512;

fn read_package(dir: &Path) -> Result<Package, String> {
    use std::io::Read;
    if !dir.is_dir() {
        return Err("The payload folder is missing. Nothing will be changed.".into());
    }
    let unreadable = || "payload.sha256 is missing or unreadable. Nothing will be changed.";
    let mut text = String::new();
    open_staged(&dir.join("payload.sha256"))
        .ok_or_else(unreadable)?
        .take(MAX_SUM_BYTES)
        .read_to_string(&mut text)
        .map_err(|_| unreadable())?;
    let token = text.split_whitespace().next().unwrap_or("").trim();
    let expected = hex_hash(token).map_err(|()| {
        "payload.sha256 isn't a SHA-256 digest. Nothing will be changed.".to_string()
    })?;
    let file = open_staged(&dir.join("payload.tar")).ok_or_else(|| {
        "payload.tar is missing or unreadable. Nothing will be changed.".to_string()
    })?;
    let architecture = Architecture::native()
        .map_err(|_| "This processor isn't a supported install target.".to_string())?;
    Package::read(file, architecture, expected).map_err(|error| {
        format!("The payload isn't a valid Crosspane archive ({error}). Nothing will be changed.")
    })
}

fn hex_hash(text: &str) -> Result<[u8; 32], ()> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(());
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
    }
    Ok(out)
}
