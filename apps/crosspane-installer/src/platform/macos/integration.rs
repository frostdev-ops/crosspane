//! The Mac production binding: native adapters on one worker thread, the agent port and the
//! practice fixture on the GUI thread, composed behind the shared `live::Platform` port.
//!
//! Nothing here decides what a result means. `worker.rs` holds that policy; `native.rs` wraps each
//! merged native module; `ports.rs` carries the GUI-thread ports, which ask the worker for a fresh
//! admission of the running agent (the proofs live five seconds) before they send; `probes.rs`
//! holds the production support and signature probes (public, read-only framework calls).
//!
//! There is no fake mode in production. A build with an embedded approved inventory, running from
//! its installer bundle in this account's home folder, composes the native domains over the
//! production probes (see `MacPlatform::native`). A build that can't (no or an invalid inventory,
//! or no usable install target) is honestly non-mutating: [`domains::Blocked`] answers every step
//! with one typed reason and changes nothing. `compose` takes the same parts from tests, whose
//! domains are in-memory.

mod diagnose;
mod domains;
pub(crate) use diagnose::diagnose;
mod native;
mod ports;
mod probes;
#[cfg(test)]
mod smoke_tests;
mod worker;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result};
use crosspane_installer_core::StepId;
use eframe::egui;

use super::fonts::discover_system_font;
use super::native_io::{Cancellation, Clock as NativeClock};
use super::payload::ApprovedInventory;
use crate::agent_contract::{AgentPlatform, AgentPort};
use crate::gui::InstallerController;
use crate::live::{
    Clock, LiveController, LiveError, NativeJob, NativeRefusal, NativeReport, NativeStep, Platform,
    PlatformDescription, PracticeFixtures, SupportChecklist, SupportChecksSlot,
};
use crate::tutorial_flow::TutorialSourcePolicy;
use crate::view::{ProgressGroup, ScreenId};

pub use domains::{
    Admitted, Agents, AudioError, AudioPackages, AudioPreview, AudioState, Blocked, BlockedBy,
    DomainFactory, Domains, FixtureChild, FixtureLauncher, InstallApplied, InstallError,
    InstallPreview, InstallState, Installs, RepairFinish, RepairOffer, RepairStep, Repairer,
    Support, SupportOutcome, UninstallOffer, UninstallResult, Uninstaller,
};
pub use native::MacProbes;
use native::NativeEnv;
pub use ports::{AgentBackend, AgentSlot, Broker, Command, MacPractice, NativeBackend};
pub use probes::{SecuritySignatures, SessionSupport};

pub const SUPPORT: StepId = StepId(10);
/// The payload, the sign-in item and the agent's first start are one native flow: the merged
/// adapters drive them together, so they are one step.
pub const INSTALL: StepId = StepId(20);
pub const AGENT: StepId = StepId(21);
pub const PERMISSIONS: StepId = StepId(30);
/// The shared sound driver, installed by macOS Installer from a package the build ships.
pub const AUDIO: StepId = StepId(31);

/// How this binary learned its approved file list. Never read from disk, payload or network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InventoryAdmission {
    Absent,
    Malformed,
    Present(Box<ApprovedInventory>),
}

/// Production entry: the system font first, then the live controller. Only `gui::run` calls
/// this, and only for a normal (non-demo) launch.
pub fn open() -> Result<(egui::FontDefinitions, Box<dyn InstallerController>)> {
    let clock = monotonic_clock();
    let font = discover_system_font().context("Cannot read a Mac system font")?;
    let font_path = font.candidate.path().to_path_buf();
    let platform = match embedded_inventory() {
        InventoryAdmission::Present(inventory) => match production_env(*inventory, &clock) {
            Ok(env) => MacPlatform::native(clock.clone(), env, font_path)?,
            Err(reason) => {
                MacPlatform::blocked(clock.clone(), reason, BlockedBy::Location, font_path)?
            }
        },
        other => MacPlatform::blocked(
            clock.clone(),
            blocked_reason(&other),
            BlockedBy::Inventory,
            font_path,
        )?,
    };
    let controller = LiveController::new(Box::new(platform), clock).map_err(live_err)?;
    Ok((font.definitions, Box::new(controller)))
}

fn monotonic_clock() -> Clock {
    let start = Instant::now();
    Arc::new(move || start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
}

fn live_err(error: LiveError) -> anyhow::Error {
    anyhow::anyhow!("{error}")
}

fn embedded_inventory() -> InventoryAdmission {
    match option_env!("CROSSPANE_MAC_APPROVED_INVENTORY") {
        None => InventoryAdmission::Absent,
        Some(text) => inventory_from_json(text),
    }
}

/// Parse the serialized `ApprovedInventory` JSON. `validate` still runs only inside `admit`.
fn inventory_from_json(text: &str) -> InventoryAdmission {
    match serde_json::from_str::<ApprovedInventory>(text) {
        Ok(inventory)
            if !inventory.product_version.is_empty()
                && !inventory.files.is_empty()
                && inventory.files.iter().all(|f| !f.path.is_empty()) =>
        {
            InventoryAdmission::Present(Box::new(inventory))
        }
        _ => InventoryAdmission::Malformed,
    }
}

/// The selected target for a production launch: this account, its GUI temporary folder, and the
/// signed payload inside the installer's own bundle (`<installer>.app/Contents/Resources/payload`).
/// The native target only admits a payload under the home or GUI temporary folder, so an
/// installer opened from a disk image or `/Applications` is refused with a typed reason.
fn production_env(
    inventory: ApprovedInventory,
    clock: &Clock,
) -> std::result::Result<NativeEnv, &'static str> {
    use super::native_io::{MacTarget, TargetPaths};
    const NOT_HERE: &str = "Open the installer from a folder in your home folder (for example         Downloads), not from a disk image or another account's folder. Nothing will be copied,         started or removed.";
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|_| NOT_HERE)?;
    // .../Installer.app/Contents/MacOS/<exe> -> .../Installer.app/Contents/Resources/payload
    let contents = exe
        .parent()
        .and_then(|macos| macos.parent())
        .filter(|c| c.file_name().is_some_and(|n| n == "Contents"))
        .ok_or(NOT_HERE)?;
    let payload_root = contents.join("Resources/payload");
    let home = PathBuf::from(objc2_foundation::NSHomeDirectory().to_string());
    let gui_tmpdir = probes::gui_tmpdir().ok_or(NOT_HERE)?;
    let target = MacTarget::selected(TargetPaths {
        uid: rustix::process::geteuid().as_raw(),
        home,
        gui_tmpdir,
        runtime_override: None,
        payload_root,
    })
    .map_err(|_| NOT_HERE)?;
    let probes = MacProbes::with_system_runner(
        Arc::new(probes::SessionSupport),
        Arc::new(probes::SecuritySignatures),
    );
    Ok(NativeEnv::new(
        target,
        inventory,
        probes,
        Arc::new(FnClock(clock.clone())),
    ))
}

/// Why a build that can't prove the Mac or Crosspane changes nothing.
fn blocked_reason(inventory: &InventoryAdmission) -> &'static str {
    match inventory {
        InventoryAdmission::Absent => {
            "This build has no approved inventory (unsigned or dev build), so nothing will be \
             copied, started or removed."
        }
        InventoryAdmission::Malformed => {
            "This build has an invalid approved inventory, so nothing will be copied, started or \
             removed."
        }
        // A present inventory is never blocked for itself; see `production_env`.
        InventoryAdmission::Present(_) => {
            "This build's approved inventory can't be used here, so nothing will be copied, \
             started or removed."
        }
    }
}

struct StepFlags {
    installed: bool,
    uses_status: bool,
    agent_apply: Option<crate::live::AgentApply>,
}

fn step(
    id: StepId,
    prerequisites: &[StepId],
    screen: ScreenId,
    group: ProgressGroup,
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
        group,
        label: label.into(),
        action_label: action.into(),
        uses_status: flags.uses_status,
        settles_with_peer: false,
        agent_apply: flags.agent_apply,
    }
}

/// The Mac readiness graph, in the shared controller's vocabulary.
///
/// Support and the install make Crosspane *installed*. The agent and the permissions are live
/// checks, each needing a Status issued for it; the permissions step asks the agent itself
/// (through the shared controller) and is only ever verified from a later Status.
pub fn description() -> PlatformDescription {
    let install = StepFlags {
        installed: true,
        uses_status: false,
        agent_apply: None,
    };
    PlatformDescription {
        platform: AgentPlatform::Macos,
        machine_label: "This Mac".into(),
        steps: vec![
            step(
                SUPPORT,
                &[],
                ScreenId::Compatibility,
                ProgressGroup::Install,
                StepFlags { ..install },
                "This Mac can run Crosspane",
                "Check this Mac",
            ),
            step(
                INSTALL,
                &[SUPPORT],
                ScreenId::InstallPlan,
                ProgressGroup::Install,
                StepFlags {
                    installed: true,
                    uses_status: true,
                    agent_apply: None,
                },
                "Crosspane is installed and starts when you sign in",
                "Install",
            ),
            step(
                AGENT,
                &[INSTALL],
                ScreenId::Installing,
                ProgressGroup::Install,
                StepFlags {
                    installed: false,
                    uses_status: true,
                    agent_apply: None,
                },
                "Crosspane is running with its key in the Keychain",
                "Check the agent",
            ),
            step(
                PERMISSIONS,
                &[AGENT],
                ScreenId::Permissions,
                ProgressGroup::PermissionsNetwork,
                StepFlags {
                    installed: false,
                    uses_status: true,
                    agent_apply: Some(crate::live::AgentApply::AskPermissions),
                },
                "Crosspane has the Mac permissions it needs",
                "Ask for permissions",
            ),
            step(
                AUDIO,
                &[AGENT],
                ScreenId::AudioComponent,
                ProgressGroup::PermissionsNetwork,
                StepFlags {
                    installed: false,
                    uses_status: true,
                    agent_apply: None,
                },
                "The Crosspane sound driver is installed",
                "Install the sound driver",
            ),
        ],
        connect_after: vec![AGENT, PERMISSIONS],
        practice_after: vec![AGENT, PERMISSIONS, AUDIO],
        hiding_choice: true,
        // Until the person chooses to hide, windows sent from this Mac are mirrored; the shared
        // controller switches to the private display once that choice is verified.
        source_policy: TutorialSourcePolicy::MacMirror,
        speakers_device: Some("crosspane.{peer}.speaker".into()),
        resume_note: None,
    }
}

/// The parts of the Mac binding. Production fills these with the native domains (or with
/// [`Blocked`] ones); tests supply in-memory ones.
pub struct Parts {
    pub clock: Clock,
    /// Built on the worker thread, which then owns every domain.
    pub domains: DomainFactory,
    pub agent: AgentSource,
    pub fixtures: FixtureSource,
}

pub enum AgentSource {
    /// The bounded socket port to the installed agent.
    Native,
    #[doc(hidden)]
    Injected(Box<dyn AgentBackend>),
}

pub enum FixtureSource {
    /// The owned practice window, started once the worker has admitted its executable.
    Native { font: PathBuf },
    #[doc(hidden)]
    Injected(Box<dyn PracticeFixtures>),
}

pub struct MacPlatform {
    desc: PlatformDescription,
    commands: Option<mpsc::SyncSender<Command>>,
    reports: Receiver<NativeReport>,
    broker: Arc<Broker>,
    agent: AgentSlot,
    fixtures: Box<dyn PracticeFixtures>,
    stop: Arc<Cancellation>,
    /// Where the support domain writes each pass's checklist (production builds only).
    checks: Option<SupportChecksSlot>,
}

macro_rules! opaque_debug {
    ($($ty:ty),+ $(,)?) => {$(
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(stringify!($ty))
            }
        }
    )+};
}

opaque_debug!(MacPlatform, Parts, AgentSource, FixtureSource);

struct FnClock(Clock);

impl NativeClock for FnClock {
    fn now_ms(&self) -> u64 {
        (self.0)()
    }
}

impl MacPlatform {
    /// A launch that can't admit anything: every domain is [`Blocked`] with the typed reason.
    fn blocked(clock: Clock, reason: &'static str, by: BlockedBy, font: PathBuf) -> Result<Self> {
        let checks = SupportChecksSlot::new(SUPPORT);
        let slot = checks.clone();
        let domains: DomainFactory =
            Box::new(move || Blocked::domains_reporting(reason, Some((slot, by))));
        Self::start(
            Parts {
                clock,
                domains,
                agent: AgentSource::Native,
                fixtures: FixtureSource::Native { font },
            },
            Some(checks),
        )
    }

    /// The production composition: the merged native adapters over `env`, built from this Mac's
    /// selected target, the production probes and the build's embedded inventory. Crate-private:
    /// an inventory is never handed in from outside the build.
    pub(crate) fn native(clock: Clock, env: NativeEnv, font: PathBuf) -> Result<Self> {
        let checks = SupportChecksSlot::new(SUPPORT);
        let domains = native::domains(env, checks.clone());
        Self::start(
            Parts {
                clock,
                domains,
                agent: AgentSource::Native,
                fixtures: FixtureSource::Native { font },
            },
            Some(checks),
        )
    }

    /// Compose from injected parts: the test seam. It exists only in this crate's own tests and
    /// when the non-default `test-hooks` feature is on (the installer's integration tests turn it
    /// on through a dev-dependency on this crate); a production build has no way to inject
    /// domains, an agent port or fixtures.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn compose(parts: Parts) -> Result<Self> {
        Self::start(parts, None)
    }

    /// Start the worker and wire the GUI-thread ports to it.
    fn start(parts: Parts, checks: Option<SupportChecksSlot>) -> Result<Self> {
        let (commands, receiver) = mpsc::sync_channel::<Command>(8);
        let (reports_tx, reports_rx) = mpsc::sync_channel::<NativeReport>(64);
        let broker = Broker::new(commands.clone());
        let stop = Arc::new(Cancellation::default());
        let backend: Box<dyn AgentBackend> = match parts.agent {
            AgentSource::Native => Box::new(NativeBackend::new(parts.clock.clone())),
            AgentSource::Injected(backend) => backend,
        };
        let fixtures: Box<dyn PracticeFixtures> = match parts.fixtures {
            FixtureSource::Native { font } => {
                Box::new(MacPractice::new(broker.clone(), parts.clock.clone(), font))
            }
            FixtureSource::Injected(fixtures) => fixtures,
        };
        let worker = worker::WorkerParts {
            clock: parts.clock.clone(),
            native_clock: Arc::new(FnClock(parts.clock.clone())),
            domains: parts.domains,
            reports: reports_tx,
            broker: broker.clone(),
            stop: stop.clone(),
        };
        thread::Builder::new()
            .name("installer-macos".into())
            .spawn(move || {
                if let Ok(worker) = worker::Worker::new(worker) {
                    worker.run(receiver);
                }
            })
            .context("Cannot start the Mac installer worker")?;
        Ok(Self {
            desc: description(),
            commands: Some(commands),
            reports: reports_rx,
            agent: AgentSlot::new(backend, broker.clone(), parts.clock),
            broker,
            fixtures,
            stop,
            checks,
        })
    }
}

impl Platform for MacPlatform {
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

    fn support_checks(&mut self) -> Option<SupportChecklist> {
        self.checks.as_ref().and_then(SupportChecksSlot::latest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_malformed_inventory_is_never_present() {
        assert!(matches!(
            inventory_from_json("not json"),
            InventoryAdmission::Malformed
        ));
        assert!(matches!(
            inventory_from_json(r#"{"product_version":"","features":[],"files":[]}"#),
            InventoryAdmission::Malformed
        ));
    }

    #[test]
    fn every_blocked_reason_says_nothing_will_change() {
        for inventory in [InventoryAdmission::Absent, InventoryAdmission::Malformed] {
            assert!(blocked_reason(&inventory).contains("nothing will be"));
        }
    }
}
