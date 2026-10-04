//! The production adapters: each wraps one merged native module and adds no policy. They
//! translate the module's typed results into the vocabulary of `domains.rs`; the worker decides
//! what those results mean.
//!
//! Everything here rests on a [`MacNativeIo`], and one is only ever built from a selected target
//! plus a support probe and a signature probe. This crate has no production probe (the merged
//! foundation deliberately ships none), so nothing in this file is constructed by a production
//! launch today: see [`super::MacPlatform::native`]. Tests build these adapters over a scratch
//! target and injected probes.
//!
//! A `MacNativeIo` is dead once the agent's runtime directory appears (it pinned the directory
//! chain it was built from), so every operation builds a fresh one from the same target.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crosspane_installer_core::{ObservationSource, OperationId};

use super::super::audio_package::{
    AudioClock, AudioOutcome, AudioPackageAttempt, AudioPackageKind, AudioPackagePlan,
    MacAudioPackage, PackageState, Presence, SystemAudioClock,
};
use super::super::launch_agent::{
    Approval, ApprovalProbe, Disabled, LaunchPhase, LaunchPlan, LaunchState, MacLaunchAgent,
    PendingLaunch, UnobservableApproval,
};
use super::super::native_io::{
    ArtifactRole, Clock as NativeClock, CommandRunner, Deadline, MacNativeIo, MacTarget,
    NativeError, NativeResult, SignatureProbe, SigningRequirement, SupportProbe, SupportProof,
    SystemCommandRunner, TrackedAgent,
};
use super::super::payload::{
    ApprovedInventory, MacPayload, PayloadRole, PayloadState, SigningRule,
};
use super::super::removal::{
    MacRemoval, MacRemovalObserver, RemovalApplyResult, RemovalChoices, RemovalCurrentReader,
    RemovalEffect, RemovalOutcome, RemovalPlan, RemovalPreview,
};
use super::super::repair::{
    MacRepair, PendingRepair, RepairActivity, RepairEffect, RepairGuidance, RepairPreview,
    RepairProgress, RepairReassessment, plan_guidance,
};
use super::super::transport::{CallerClock, MacAgentPort, SelectedAgent, SelectedLink};
use super::domains::{
    Admitted, AdmittedInner, Agents, AudioError, AudioPackages, AudioPreview, AudioState,
    DomainFactory, Domains, FixtureChild, FixtureChildInner, FixtureLauncher, InstallApplied,
    InstallError, InstallPreview, InstallState, Installs, RepairFinish, RepairOffer, RepairStep,
    Repairer, Support, SupportOutcome, UninstallOffer, UninstallResult, Uninstaller,
};
use crate::agent_contract::{
    AgentCall, AgentPort, AgentReply, DecodedReply, InstallerRequest, StatusAdmission,
};
use crate::live::{Availability, MaintenanceOutcome, RemovalChoice, RepairOutcome as Shown};
use crate::view::ToggleRole;

/// The probes a production build must bring: the foundation provides none of the first two.
#[derive(Clone)]
pub struct MacProbes {
    pub support: Arc<dyn SupportProbe>,
    pub signatures: Arc<dyn SignatureProbe>,
    pub approval: Arc<dyn ApprovalProbe>,
    pub runner: Arc<dyn CommandRunner>,
}

impl MacProbes {
    /// The support and signature probes with the system command runner and the honest
    /// "can't observe login-item approval" probe.
    pub fn with_system_runner(
        support: Arc<dyn SupportProbe>,
        signatures: Arc<dyn SignatureProbe>,
    ) -> Self {
        Self {
            support,
            signatures,
            approval: Arc::new(UnobservableApproval),
            runner: Arc::new(SystemCommandRunner),
        }
    }
}

/// Everything the adapters share. `target` is the selected target (or a scratch one in tests).
#[derive(Clone)]
pub(crate) struct NativeEnv {
    pub(crate) target: MacTarget,
    /// The approved file list embedded in this build: never read from the payload, and never
    /// handed in from outside the crate.
    pub(crate) inventory: ApprovedInventory,
    pub(crate) probes: MacProbes,
    pub(crate) clock: Arc<dyn NativeClock>,
    pub(crate) audio_clock: Arc<dyn AudioClock>,
}

impl NativeEnv {
    pub(crate) fn new(
        target: MacTarget,
        inventory: ApprovedInventory,
        probes: MacProbes,
        clock: Arc<dyn NativeClock>,
    ) -> Self {
        Self {
            target,
            inventory,
            probes,
            clock,
            audio_clock: Arc::new(SystemAudioClock),
        }
    }

    fn io(&self) -> NativeResult<Arc<MacNativeIo>> {
        MacNativeIo::new(
            self.target.clone(),
            self.probes.runner.clone(),
            self.probes.support.clone(),
            self.probes.signatures.clone(),
            self.clock.clone(),
        )
        .map(Arc::new)
    }

    fn rule(&self, path: &str) -> NativeResult<SigningRequirement> {
        let rule = self
            .inventory
            .files
            .iter()
            .find(|f| f.path == path)
            .and_then(|f| f.signing.as_ref())
            .ok_or(NativeError::Invalid)?;
        Ok(requirement(rule))
    }

    fn agent_requirement(&self) -> NativeResult<SigningRequirement> {
        self.rule("Crosspane.app/Contents/MacOS/Crosspane")
    }

    fn caller_clock(&self) -> CallerClock {
        let clock = self.clock.clone();
        Arc::new(move || clock.now_ms())
    }
}

fn requirement(rule: &SigningRule) -> SigningRequirement {
    SigningRequirement {
        role: match rule.role {
            PayloadRole::Agent => ArtifactRole::Agent,
            PayloadRole::Settings => ArtifactRole::Settings,
            PayloadRole::Tutorial => ArtifactRole::Tutorial,
            PayloadRole::Ctl => ArtifactRole::Ctl,
            PayloadRole::Installer => ArtifactRole::Installer,
            PayloadRole::EmbeddedCode => ArtifactRole::EmbeddedCode,
        },
        identifier: rule.identifier.clone(),
        designated_requirement: rule.designated_requirement.clone(),
        entitlements: rule.entitlements.clone(),
    }
}

/// The native domains over `env`, built on the worker thread.
pub fn domains(env: NativeEnv) -> DomainFactory {
    let env = Arc::new(env);
    Box::new(move || Domains {
        support: Box::new(NativeSupport { env: env.clone() }),
        installs: Box::new(NativeInstalls::new(env.clone())),
        audio: Box::new(NativeAudio::new(env.clone())),
        agents: Box::new(NativeAgents { env: env.clone() }),
        uninstaller: Box::new(NativeUninstaller::new(env.clone())),
        repairer: Box::new(NativeRepairer::new(env.clone())),
        fixtures: Some(Box::new(NativeFixtures { env })),
    })
}

/// Admit the running agent right now: its signature, then support, then the instance.
fn admit_selected(
    env: &NativeEnv,
    link: Option<SelectedLink>,
    deadline: &Deadline,
) -> NativeResult<SelectedAgent> {
    let io = env.io()?;
    let requirement = env.agent_requirement()?;
    let main = io.admit_main_signature(&io.target().agent_path(), &requirement, deadline)?;
    let support = io.admit_support(&main, deadline)?;
    let instance = io.admit_instance(&support, &main, deadline)?;
    Ok(SelectedAgent {
        io,
        support,
        instance: Arc::new(instance),
        link,
    })
}

// ---- support ----------------------------------------------------------------------------------

pub struct NativeSupport {
    env: Arc<NativeEnv>,
}

impl Support for NativeSupport {
    fn observe(&mut self, deadline: &Deadline) -> SupportOutcome {
        let attempt = (|| {
            let io = self.env.io()?;
            let requirement = self.env.agent_requirement()?;
            let incoming = io
                .target()
                .paths()
                .payload_root
                .join("Crosspane.app/Contents/MacOS/Crosspane");
            let main = io.admit_main_signature(&incoming, &requirement, deadline)?;
            io.admit_support(&main, deadline)?;
            Ok::<_, NativeError>(io.target().source())
        })();
        match attempt {
            Ok(source) => SupportOutcome::Supported(source),
            Err(NativeError::Unsupported) => SupportOutcome::Unsupported(
                "This Mac isn't supported, or this copy of Crosspane isn't the signed release this \
                 build expects. Crosspane needs macOS 26 or newer on Apple silicon with an active \
                 signed-in session. Nothing will be changed."
                    .into(),
            ),
            Err(_) => SupportOutcome::Unavailable(
                "This Mac's session or Crosspane's signature couldn't be proved right now. \
                 Nothing will be changed."
                    .into(),
            ),
        }
    }
}

// ---- agent admission --------------------------------------------------------------------------

pub struct NativeAgents {
    env: Arc<NativeEnv>,
}

impl Agents for NativeAgents {
    fn admit(
        &mut self,
        link: Option<SelectedLink>,
        deadline: &Deadline,
    ) -> Result<Admitted, String> {
        admit_selected(&self.env, link, deadline)
            .map(|selected| Admitted {
                inner: AdmittedInner::Native(Box::new(selected)),
            })
            .map_err(|_| "The running agent couldn't be admitted.".to_owned())
    }
}

// ---- install ----------------------------------------------------------------------------------

/// A plan that was previewed and is waiting for consent.
struct Kept {
    operation: OperationId,
    launch: MacLaunchAgent,
    plan: LaunchPlan,
}

/// An install whose first start was requested and is waiting to be confirmed from the agent.
struct Running {
    launch: MacLaunchAgent,
    pending: PendingLaunch,
}

pub struct NativeInstalls {
    env: Arc<NativeEnv>,
    kept: Option<Kept>,
    running: Option<Running>,
}

fn blocker(state: LaunchState) -> Option<InstallError> {
    match state {
        LaunchState::Conflict | LaunchState::AdoptionRequired => Some(InstallError::Foreign),
        LaunchState::UserDisabled => Some(InstallError::UserDisabled),
        LaunchState::Unobservable => Some(InstallError::Unobservable),
        LaunchState::Absent | LaunchState::Owned => None,
    }
}

fn map_native(error: NativeError) -> InstallError {
    match error {
        NativeError::Foreign | NativeError::Refused | NativeError::Invalid => InstallError::Refused,
        NativeError::Unsupported => InstallError::Unsupported,
        NativeError::OutcomeUnknown => InstallError::OutcomeUnknown,
        NativeError::Unavailable | NativeError::Busy => InstallError::Unavailable,
        NativeError::Timeout | NativeError::Cancelled => InstallError::Unavailable,
        NativeError::Oversize | NativeError::IdExhausted => InstallError::Failed,
    }
}

/// A payload or inventory that can't be admitted means this build can't install from what is next to
/// it: that is a blocked build, not a transient failure.
fn map_admission(error: NativeError) -> InstallError {
    match error {
        NativeError::Invalid
        | NativeError::Foreign
        | NativeError::Unsupported
        | NativeError::Oversize => InstallError::Blocked(
            "The Crosspane files next to this installer don't match what this build approved, or \
             this Mac can't install them. Nothing will be changed."
                .into(),
        ),
        other => map_native(other),
    }
}

impl NativeInstalls {
    pub fn new(env: Arc<NativeEnv>) -> Self {
        Self {
            env,
            kept: None,
            running: None,
        }
    }

    /// The running agent and the Status that came with the job, if both are usable. Without
    /// them a running launchd job can't be planned against.
    fn current(
        &self,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Option<(SelectedAgent, AgentReply)> {
        let reply = status?;
        if !matches!(
            reply.result,
            Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
        ) {
            return None;
        }
        let selected = admit_selected(&self.env, None, deadline).ok()?;
        Some((selected, reply.clone()))
    }

    /// Admit the launch flow and plan it for `operation`.
    fn plan_launch(
        &self,
        operation: OperationId,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<(MacLaunchAgent, LaunchPlan, bool), InstallError> {
        let io = self.env.io().map_err(map_native)?;
        let mut launch = MacLaunchAgent::admit(
            io.clone(),
            self.env.inventory.clone(),
            self.env.probes.approval.clone(),
            deadline,
        )
        .map_err(map_admission)?;
        let current = self.current(status, deadline);
        let current_ref = current.as_ref().map(|(agent, reply)| (agent, reply));
        let plan = launch
            .plan(operation.0, operation.0, current_ref, deadline)
            .map_err(map_native)?;
        // The payload's own state: only the payload adapter knows whether its record was
        // verified, which is what makes the install "current".
        let payload =
            MacPayload::admit(io, self.env.inventory.clone(), deadline).map_err(map_admission)?;
        let payload_state = payload
            .plan(operation.0, operation.0, None, deadline)
            .map_err(map_native)?
            .state();
        if payload_state == PayloadState::Conflict
            || payload_state == PayloadState::AdoptionRequired
        {
            return Err(InstallError::Foreign);
        }
        let is_current =
            plan.state() == LaunchState::Owned && payload_state == PayloadState::Matching;
        Ok((launch, plan, is_current))
    }
}

impl Installs for NativeInstalls {
    fn detect(
        &mut self,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<InstallState, InstallError> {
        let operation = OperationId(1);
        let (_, plan, current) = self.plan_launch(operation, status, deadline)?;
        if let Some(error) = blocker(plan.state()) {
            return Err(error);
        }
        Ok(if current {
            InstallState::Current
        } else {
            InstallState::Needed
        })
    }

    fn plan(
        &mut self,
        operation: OperationId,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<Option<InstallPreview>, InstallError> {
        self.kept = None;
        let (launch, plan, current) = self.plan_launch(operation, status, deadline)?;
        if let Some(error) = blocker(plan.state()) {
            return Err(error);
        }
        if current {
            return Ok(None);
        }
        let preview = InstallPreview {
            version: self.env.inventory.product_version.clone(),
            interrupts_agent: plan.interrupts_agent(),
        };
        self.kept = Some(Kept {
            operation,
            launch,
            plan,
        });
        Ok(Some(preview))
    }

    fn apply(
        &mut self,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<InstallApplied, InstallError> {
        let Some(kept) = self.kept.take().filter(|k| k.operation == operation) else {
            return Err(InstallError::Refused);
        };
        // Adopting something Crosspane didn't create is never part of this consent.
        let consent = kept
            .plan
            .consent(operation.0, operation.0, false, false)
            .map_err(|_| InstallError::Foreign)?;
        let mut pending = match kept.launch.execute(kept.plan, consent, deadline) {
            Ok(pending) => pending,
            Err(NativeError::Timeout | NativeError::Cancelled) => {
                return Err(InstallError::OutcomeUnknown);
            }
            Err(error) => return Err(map_native(error)),
        };
        // The old agent stops first; wait for that, within this same bounded apply.
        while pending.phase() == LaunchPhase::WaitingForCleanStop {
            if deadline.check().is_err() {
                self.running = Some(Running {
                    launch: kept.launch,
                    pending,
                });
                return Ok(InstallApplied::Unknown);
            }
            thread::sleep(Duration::from_millis(250));
            if kept
                .launch
                .resume_clean_stop(&mut pending, deadline)
                .is_err()
            {
                break;
            }
        }
        let outcome = match pending.phase() {
            LaunchPhase::BootstrapRequested | LaunchPhase::Observed => InstallApplied::Requested,
            _ => InstallApplied::Unknown,
        };
        self.running = Some(Running {
            launch: kept.launch,
            pending,
        });
        Ok(outcome)
    }

    fn verify(
        &mut self,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<ObservationSource, InstallError> {
        let reply = status
            .filter(|r| {
                matches!(
                    r.result,
                    Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
                )
            })
            .ok_or(InstallError::Unavailable)?;
        let Some(mut running) = self.running.take() else {
            // Nothing was applied in this run: the install counts only if it is current now,
            // judged from a fresh read against this very Status.
            return match self.detect(status, deadline)? {
                InstallState::Current => Ok(reply.source),
                InstallState::Needed => Err(InstallError::Unavailable),
            };
        };
        let selected = admit_selected(&self.env, None, deadline).map_err(map_native)?;
        // The Status was issued after the apply: say so to the adapter, which refuses anything
        // issued earlier or from the old instance.
        running
            .pending
            .expect_health(reply.id)
            .map_err(|_| InstallError::Unavailable)?;
        match running.launch.observe(
            &mut running.pending,
            &selected,
            reply.clone(),
            None,
            deadline,
        ) {
            Ok(facts) => {
                if facts.approval == Approval::Denied || facts.disabled == Disabled::Yes {
                    return Err(InstallError::UserDisabled);
                }
                Ok(facts.reply.source)
            }
            Err(error) => {
                // Not confirmed yet: keep the flow so a later Status can confirm it.
                self.running = Some(running);
                Err(map_native(error))
            }
        }
    }
}

// ---- audio driver package ---------------------------------------------------------------------

/// A plan that was previewed and is waiting for consent.
struct KeptAudio {
    operation: OperationId,
    io: Arc<MacNativeIo>,
    package: MacAudioPackage,
    plan: AudioPackagePlan,
}

/// The installer was opened and its outcome is waited for.
struct OpenedAudio {
    io: Arc<MacNativeIo>,
    package: MacAudioPackage,
    attempt: AudioPackageAttempt,
}

pub struct NativeAudio {
    env: Arc<NativeEnv>,
    kept: Option<KeptAudio>,
    opened: Option<OpenedAudio>,
}

fn audio_error(error: NativeError) -> AudioError {
    match error {
        NativeError::Invalid
        | NativeError::Foreign
        | NativeError::Unsupported
        | NativeError::Oversize => AudioError::Blocked(
            "The sound driver package next to this installer doesn't match what this build \
             expects. Nothing will be changed."
                .into(),
        ),
        NativeError::Refused => AudioError::Refused(
            "The sound driver can't be installed from here right now. Nothing was changed.".into(),
        ),
        NativeError::OutcomeUnknown => AudioError::Unknown,
        _ => AudioError::Unavailable,
    }
}

impl NativeAudio {
    pub fn new(env: Arc<NativeEnv>) -> Self {
        Self {
            env,
            kept: None,
            opened: None,
        }
    }

    /// A fresh package adapter and the support proof it needs, read in one tight sequence: the
    /// proof lives five seconds.
    fn fresh(
        &self,
        deadline: &Deadline,
    ) -> NativeResult<(Arc<MacNativeIo>, MacAudioPackage, SupportProof)> {
        let io = self.env.io()?;
        let requirement = self.env.agent_requirement()?;
        let incoming = io
            .target()
            .paths()
            .payload_root
            .join("Crosspane.app/Contents/MacOS/Crosspane");
        let main = io.admit_main_signature(&incoming, &requirement, deadline)?;
        let support = io.admit_support(&main, deadline)?;
        let source = io
            .target()
            .paths()
            .payload_root
            .join("Crosspane.app/Contents/Resources/audio");
        let package =
            MacAudioPackage::admit(io.clone(), source, self.env.audio_clock.clone(), deadline)?;
        Ok((io, package, support))
    }

    /// The driver as the folders show it right now.
    fn presence(&self, deadline: &Deadline) -> Result<(Presence, bool), AudioError> {
        let (_, mut package, support) = self.fresh(deadline).map_err(audio_error)?;
        let plan = package
            .plan(&support, AudioPackageKind::Install, 1, 1, deadline)
            .map_err(audio_error)?;
        Ok((plan.preview().driver, plan.preview().safe_metadata))
    }

    /// What the installer's own outcome says about the attempt in hand.
    fn outcome(opened: &OpenedAudio) -> Result<AudioState, AudioError> {
        let facts = opened.attempt.facts();
        match facts.state {
            PackageState::Outcome(AudioOutcome::Installed) => Ok(AudioState::Installed),
            PackageState::Outcome(AudioOutcome::VerifyFailed) => Err(AudioError::Failed(
                "The installer reported that it couldn't verify the sound driver. An earlier copy, \
                 if there was one, is kept for manual recovery."
                    .into(),
            )),
            PackageState::Outcome(AudioOutcome::MoveFailed) => Err(AudioError::Failed(
                "The installer couldn't put the sound driver in place. An earlier copy, if there \
                 was one, is kept for manual recovery."
                    .into(),
            )),
            PackageState::Outcome(_) => Err(AudioError::Unknown),
            PackageState::OpenRequested => Ok(AudioState::InProgress),
            // No outcome file yet is the normal state while the installer is still open.
            PackageState::Unknown => match facts.error {
                None | Some(NativeError::OutcomeUnknown) => Ok(AudioState::InProgress),
                Some(_) => Err(AudioError::Unknown),
            },
        }
    }

    fn observe(&mut self, deadline: &Deadline) -> Option<Result<AudioState, AudioError>> {
        let opened = self.opened.as_mut()?;
        if let Err(error) = opened.package.observe(&mut opened.attempt, deadline) {
            return Some(Err(audio_error(error)));
        }
        Some(Self::outcome(opened))
    }
}

impl AudioPackages for NativeAudio {
    fn detect(&mut self, deadline: &Deadline) -> Result<AudioState, AudioError> {
        if let Some(state) = self.observe(deadline) {
            return state;
        }
        match self.presence(deadline)? {
            (Presence::Present, true) => Ok(AudioState::Installed),
            (Presence::Absent, _) | (Presence::Present, false) => Ok(AudioState::Needed),
            (Presence::Unknown, _) => Err(AudioError::Unavailable),
        }
    }

    fn plan(
        &mut self,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<AudioPreview, AudioError> {
        self.kept = None;
        let (io, mut package, support) = self.fresh(deadline).map_err(audio_error)?;
        let plan = package
            .plan(
                &support,
                AudioPackageKind::Install,
                operation.0,
                operation.0,
                deadline,
            )
            .map_err(audio_error)?;
        let preview = plan.preview().clone();
        // The adapter refuses consent for folders it can't vouch for; say so now, not at the click.
        if !preview.safe_metadata || preview.same_volume != Some(true) {
            return Err(AudioError::Refused(
                "The folders the sound driver goes in aren't as Crosspane expects (owned by the \
                 system, not writable by others, on one volume), so it won't be installed. \
                 Nothing was changed."
                    .into(),
            ));
        }
        let facts = AudioPreview {
            version: package.version().to_owned(),
            interrupts_system_audio: preview.interrupts_system_audio,
            keeps_previous: preview.previous == Presence::Present,
        };
        self.kept = Some(KeptAudio {
            operation,
            io,
            package,
            plan,
        });
        Ok(facts)
    }

    fn apply(&mut self, operation: OperationId, deadline: &Deadline) -> Result<(), AudioError> {
        let Some(mut kept) = self.kept.take().filter(|k| k.operation == operation) else {
            return Err(AudioError::Refused(
                "That preview is no longer current. Review the sound driver again.".into(),
            ));
        };
        // The consent names the shared, every-user driver and the interruption, and keeps any
        // earlier copy: the preview said all three.
        let consent = kept
            .plan
            .consent(operation.0, operation.0, true, false)
            .map_err(audio_error)?;
        let requirement = self.env.agent_requirement().map_err(audio_error)?;
        let incoming = kept
            .io
            .target()
            .paths()
            .payload_root
            .join("Crosspane.app/Contents/MacOS/Crosspane");
        let main = kept
            .io
            .admit_main_signature(&incoming, &requirement, deadline)
            .map_err(audio_error)?;
        let support = kept
            .io
            .admit_support(&main, deadline)
            .map_err(audio_error)?;
        let attempt = kept
            .package
            .open(kept.plan, consent, &support, deadline)
            .map_err(audio_error)?;
        match attempt.facts().state {
            PackageState::OpenRequested => {
                self.opened = Some(OpenedAudio {
                    io: kept.io,
                    package: kept.package,
                    attempt,
                });
                Ok(())
            }
            _ => Err(AudioError::Refused(
                "macOS didn't open the installer, so nothing was installed.".into(),
            )),
        }
    }

    fn verify(&mut self, deadline: &Deadline) -> Result<ObservationSource, AudioError> {
        let Some(state) = self.observe(deadline) else {
            // Nothing was opened in this run: the driver counts only if it is in place now.
            let io = self.env.io().map_err(audio_error)?;
            return match self.detect(deadline)? {
                AudioState::Installed => Ok(io.target().source()),
                _ => Err(AudioError::Unavailable),
            };
        };
        match state? {
            AudioState::Installed => {
                // The installer's outcome is read alongside the folders: both must agree.
                let source = self
                    .opened
                    .as_ref()
                    .map(|o| o.io.target().source())
                    .ok_or(AudioError::Unavailable)?;
                match self.presence(deadline)? {
                    (Presence::Present, _) => {
                        self.opened = None;
                        Ok(source)
                    }
                    _ => Err(AudioError::Unknown),
                }
            }
            AudioState::InProgress => Err(AudioError::Waiting),
            AudioState::Needed => Err(AudioError::Unavailable),
        }
    }
}

// ---- uninstall --------------------------------------------------------------------------------

const DELETE_IDENTITY: u16 = 1;
const REMOVE_DRIVER: u16 = 2;

struct PlannedRemoval {
    operation: OperationId,
    choices: RemovalChoices,
    deltas: Vec<(String, RemovalEffect)>,
    /// Exactly the text the person confirmed, including every interruption it named.
    shown: String,
    /// The running agent instance the preview was observed against, if any.
    instance: Option<u64>,
}

pub struct NativeUninstaller {
    env: Arc<NativeEnv>,
    planned: Option<PlannedRemoval>,
    ids: Arc<AtomicU64>,
}

fn effect_phrase(effect: RemovalEffect) -> Option<&'static str> {
    Some(match effect {
        RemovalEffect::DisableOwnedAutostart => "turn off Crosspane's sign-in item",
        RemovalEffect::StopTrackedAgent => {
            "stop the running Crosspane (any session in progress ends)"
        }
        RemovalEffect::EraseOnlyAfterCleanExit => {
            "erase this Mac's Crosspane identity and pairings, only after the agent has exited \
             cleanly"
        }
        RemovalEffect::RemoveSharedDriverAfterPackageVerification => {
            "remove the shared Crosspane audio driver"
        }
        RemovalEffect::RemovePreviousAfterPackageVerification => {
            "remove the previous audio driver copy"
        }
        RemovalEffect::RemoveOwnedAfterVerification => "remove Crosspane's own files",
        RemovalEffect::KeepRecovery => "keep the recovery files",
        RemovalEffect::KeepIdentity => "keep this Mac's identity and pairings",
        RemovalEffect::KeepForeign => "leave files Crosspane didn't create untouched",
        RemovalEffect::PruneEmptyOwnedAfterVerification | RemovalEffect::Absent => return None,
    })
}

fn render(preview: &RemovalPreview) -> String {
    let mut phrases: Vec<&'static str> = Vec::new();
    for delta in &preview.deltas {
        if let Some(phrase) = effect_phrase(delta.effect)
            && !phrases.contains(&phrase)
        {
            phrases.push(phrase);
        }
    }
    let mut text = format!(
        "Remove Crosspane from this account: {}.",
        phrases.join("; ")
    );
    if preview.audio.interrupts_system_audio {
        text.push_str(" Removing the audio driver briefly interrupts this Mac's sound.");
    }
    text.push(' ');
    text.push_str(preview.identity_explanation);
    text
}

fn deltas_of(preview: &RemovalPreview) -> Vec<(String, RemovalEffect)> {
    preview
        .deltas
        .iter()
        .map(|d| (d.resource.clone(), d.effect))
        .collect()
}

fn choices_of(selected: &[(u16, bool)]) -> RemovalChoices {
    let mut choices = RemovalChoices::default();
    for (id, checked) in selected {
        match *id {
            DELETE_IDENTITY => choices.delete_identity = *checked,
            REMOVE_DRIVER => choices.remove_driver = *checked,
            _ => {}
        }
    }
    choices
}

/// Fresh Status replies for the removal coordinator, from a port that is admitted just for the
/// read. The ids sit far above anything the controller issues, and only ever increase.
struct NativeReader {
    env: Arc<NativeEnv>,
    ids: Arc<AtomicU64>,
}

impl RemovalCurrentReader for NativeReader {
    fn read(
        &self,
        _original: &TrackedAgent,
        deadline: &Deadline,
    ) -> NativeResult<(SelectedAgent, AgentReply)> {
        read_status(&self.env, &self.ids, deadline)
    }
}

/// A fresh Status from the running agent, through a port admitted just for this read. The call
/// ids come from `ids` and only ever increase, so the coordinators' monotonic receipts accept
/// each read after the one before it.
fn read_status(
    env: &NativeEnv,
    ids: &AtomicU64,
    deadline: &Deadline,
) -> NativeResult<(SelectedAgent, AgentReply)> {
    let selected = admit_selected(env, None, deadline)?;
    let mut port = MacAgentPort::new(selected.clone(), env.caller_clock())?;
    let id = ids.fetch_add(1, Ordering::Relaxed);
    let timeout_ms = deadline.remaining_ms()?.clamp(1, 5_000);
    port.submit(AgentCall {
        id,
        request: InstallerRequest::Status,
        timeout_ms,
    })
    .map_err(|_| NativeError::Unavailable)?;
    loop {
        deadline.check()?;
        for reply in port.poll() {
            if reply.id == id {
                return match reply.result {
                    Ok(_) => Ok((selected, reply)),
                    Err(_) => Err(NativeError::Unavailable),
                };
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
}

const READER_IDS_BASE: u64 = 1 << 40;

impl NativeUninstaller {
    pub fn new(env: Arc<NativeEnv>) -> Self {
        Self {
            env,
            planned: None,
            ids: Arc::new(AtomicU64::new(READER_IDS_BASE)),
        }
    }

    fn removal_plan(
        &self,
        operation: OperationId,
        choices: RemovalChoices,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> NativeResult<(MacRemoval, RemovalPlan)> {
        let io = self.env.io()?;
        let audio_source = io
            .target()
            .paths()
            .payload_root
            .join("Crosspane.app/Contents/Resources/audio");
        let observer = MacRemovalObserver::admit(
            io,
            self.env.inventory.clone(),
            audio_source,
            self.env.audio_clock.clone(),
            deadline,
        )?;
        let mut removal = MacRemoval::new(observer);
        let current = match status {
            Some(reply)
                if matches!(
                    reply.result,
                    Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
                ) =>
            {
                admit_selected(&self.env, None, deadline)
                    .ok()
                    .map(|selected| (selected, reply.clone()))
            }
            _ => None,
        };
        let plan = removal.plan(operation.0, operation, choices, current, deadline)?;
        Ok((removal, plan))
    }
}

fn why(error: NativeError) -> String {
    match error {
        NativeError::Foreign | NativeError::Refused => {
            "What Crosspane would remove no longer matches what was checked. Nothing was removed; \
             review again."
        }
        NativeError::Unavailable | NativeError::Busy => {
            "Crosspane's install can't be read just now. Nothing was removed."
        }
        NativeError::Timeout | NativeError::Cancelled => {
            "The removal ran out of time before it started. Nothing was removed."
        }
        _ => "The removal couldn't be planned. Nothing was removed.",
    }
    .to_owned()
}

fn outcome_word(outcome: RemovalOutcome) -> &'static str {
    match outcome {
        RemovalOutcome::Completed => "done",
        RemovalOutcome::Absent => "was already gone",
        RemovalOutcome::Kept => "kept",
        RemovalOutcome::NotClean => "not done, because the agent didn't exit cleanly",
        RemovalOutcome::Waiting => "still waiting",
        RemovalOutcome::Refused => "refused",
        RemovalOutcome::Failed => "failed",
        RemovalOutcome::Unknown => "outcome unknown",
        RemovalOutcome::NotDispatched => "not started",
        RemovalOutcome::Pending => "not started",
    }
}

fn report(result: &RemovalApplyResult) -> UninstallResult {
    let ok = |o: RemovalOutcome| {
        matches!(
            o,
            RemovalOutcome::Completed | RemovalOutcome::Absent | RemovalOutcome::Kept
        )
    };
    let done = result
        .rows
        .iter()
        .filter(|r| r.outcome == RemovalOutcome::Completed)
        .count();
    let mut lines: Vec<String> = result
        .rows
        .iter()
        .filter(|r| !ok(r.outcome))
        .filter_map(|r| {
            effect_phrase(r.delta.effect).map(|p| format!("{p}: {}", outcome_word(r.outcome)))
        })
        .collect();
    if result.complete && result.error.is_none() {
        lines.insert(0, "Crosspane was removed from this account.".into());
    } else {
        lines.insert(
            0,
            format!("{done} step(s) finished; the removal did not complete."),
        );
    }
    if result.retained_recovery {
        lines.push("Recovery tools are kept so the rest can be finished.".into());
    }
    let outcome = if result.complete && result.error.is_none() {
        MaintenanceOutcome::Removed
    } else if done > 0 {
        MaintenanceOutcome::Partial
    } else if matches!(
        result.error,
        Some(NativeError::Refused | NativeError::Foreign)
    ) {
        MaintenanceOutcome::Refused
    } else {
        MaintenanceOutcome::Failed
    };
    UninstallResult { outcome, lines }
}

impl Uninstaller for NativeUninstaller {
    fn inspect(&mut self, deadline: &Deadline) -> UninstallOffer {
        let uninstall = match self.env.io().and_then(|io| {
            let audio_source = io
                .target()
                .paths()
                .payload_root
                .join("Crosspane.app/Contents/Resources/audio");
            MacRemovalObserver::admit(
                io,
                self.env.inventory.clone(),
                audio_source,
                self.env.audio_clock.clone(),
                deadline,
            )
        }) {
            Ok(_) => Availability::Available,
            Err(_) => Availability::Unavailable(
                "Crosspane's install can't be read just now, so removal isn't offered. Nothing \
                 was changed."
                    .into(),
            ),
        };
        let choices = if matches!(uninstall, Availability::Available) {
            vec![
                RemovalChoice {
                    id: DELETE_IDENTITY,
                    role: ToggleRole::DeleteIdentity,
                    label: "Also forget this Mac's pairings".into(),
                    checked: false,
                    enabled: true,
                },
                RemovalChoice {
                    id: REMOVE_DRIVER,
                    role: ToggleRole::RemoveAudioDriver,
                    label: "Remove the Crosspane audio driver".into(),
                    checked: true,
                    enabled: true,
                },
            ]
        } else {
            Vec::new()
        };
        UninstallOffer { uninstall, choices }
    }

    fn plan(
        &mut self,
        choices: &[(u16, bool)],
        operation: OperationId,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<String, String> {
        self.planned = None;
        let chosen = choices_of(choices);
        let (_, plan) = self
            .removal_plan(operation, chosen, status, deadline)
            .map_err(why)?;
        let preview = plan.preview();
        let shown = render(preview);
        self.planned = Some(PlannedRemoval {
            operation,
            choices: chosen,
            deltas: deltas_of(preview),
            shown: shown.clone(),
            instance: preview.activity.as_ref().map(|a| a.instance),
        });
        Ok(shown)
    }

    fn apply(
        &mut self,
        operation: OperationId,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<UninstallResult, String> {
        let Some(planned) = self.planned.take().filter(|p| p.operation == operation) else {
            return Err("Confirm the current preview before removing Crosspane.".into());
        };
        // The observation behind a preview lives five seconds, and a person takes longer than
        // that to confirm. So everything is observed again now, and the removal runs only if it
        // still previews exactly what was shown.
        let fresh = OperationId(operation.0.saturating_add(1));
        let (mut removal, plan) = self
            .removal_plan(fresh, planned.choices, status, deadline)
            .map_err(why)?;
        // Everything the person read must still hold: the effects, the interruptions the text
        // named (the running agent, the audio driver) and the agent instance it was about.
        if deltas_of(plan.preview()) != planned.deltas
            || render(plan.preview()) != planned.shown
            || plan.preview().activity.as_ref().map(|a| a.instance) != planned.instance
        {
            return Err(
                "Things changed since the preview was shown. Nothing was removed; review the \
                 removal again."
                    .into(),
            );
        }
        let preview = plan.preview().clone();
        // The fresh preview renders to exactly the text the person confirmed, which names every
        // interruption (the running agent, the audio driver); that confirmation is the
        // acknowledgement.
        let consent = removal
            .consent(
                &plan,
                preview.revision,
                preview.operation,
                planned.choices,
                true,
                deadline,
            )
            .map_err(why)?;
        let reader: Arc<dyn RemovalCurrentReader> = Arc::new(NativeReader {
            env: self.env.clone(),
            ids: self.ids.clone(),
        });
        let result = removal
            .apply(&plan, &consent, Some(reader), deadline)
            .map_err(why)?;
        Ok(report(&result))
    }
}

// ---- repair -----------------------------------------------------------------------------------

/// The coordinator's monotonic receipts need every Status to be newer than the one before it,
/// across plan, apply and the later health look. All of a repair's own reads come from one
/// counter, far above anything the controller issues.
const REPAIR_IDS_BASE: u64 = 1 << 41;

/// What the person was shown: the repair starts only if a fresh plan still has this basis.
struct KeptRepair {
    operation: OperationId,
    effects: Vec<RepairEffect>,
    activity: Option<RepairActivity>,
}

/// A repair whose change was dispatched and which waits for the next stage.
struct ActiveRepair {
    repair: MacRepair,
    pending: PendingRepair,
}

pub struct NativeRepairer {
    env: Arc<NativeEnv>,
    kept: Option<KeptRepair>,
    running: Option<ActiveRepair>,
    ids: Arc<AtomicU64>,
}

impl NativeRepairer {
    pub fn new(env: Arc<NativeEnv>) -> Self {
        Self {
            env,
            kept: None,
            running: None,
            ids: Arc::new(AtomicU64::new(REPAIR_IDS_BASE)),
        }
    }

    fn admit(&self, deadline: &Deadline) -> Result<MacRepair, String> {
        let io = self.env.io().map_err(admission_text)?;
        if let Some(record) = MacRepair::saved_record(&io, &self.env.inventory, deadline)
            .map_err(|_| unreadable_record_text())?
        {
            let next = record
                .status_watermark()
                .checked_add(1)
                .ok_or_else(|| admission_text(NativeError::IdExhausted))?;
            self.ids.fetch_max(next, Ordering::Relaxed);
        }
        MacRepair::admit(
            io,
            self.env.inventory.clone(),
            self.env.probes.approval.clone(),
            deadline,
        )
        .map_err(admission_text)
    }

    /// The Status the click carried when it is usable, else one read now.
    fn current(
        &self,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Option<(SelectedAgent, AgentReply)> {
        let floor = self
            .env
            .io()
            .ok()
            .and_then(|io| {
                MacRepair::saved_record(&io, &self.env.inventory, deadline)
                    .ok()
                    .flatten()
            })
            .map_or(0, |record| record.status_watermark());
        match status.filter(|r| {
            r.id > floor
                && matches!(
                    r.result,
                    Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
                )
        }) {
            Some(reply) => {
                let selected = admit_selected(&self.env, None, deadline).ok()?;
                Some((selected, reply.clone()))
            }
            None => self.read_current(deadline).ok(),
        }
    }

    /// A persisted record (or one that can't be read) blocks a new plan or apply: it is
    /// reassessed through Resume, never overwritten by a stale or replayed request.
    fn no_saved_record(&self, deadline: &Deadline) -> Result<(), String> {
        let io = self.env.io().map_err(admission_text)?;
        match MacRepair::saved_record(&io, &self.env.inventory, deadline) {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err(unfinished_text()),
            Err(_) => Err(unreadable_record_text()),
        }
    }

    fn read_current(&self, deadline: &Deadline) -> NativeResult<(SelectedAgent, AgentReply)> {
        let io = self.env.io()?;
        if let Some(record) = MacRepair::saved_record(&io, &self.env.inventory, deadline)? {
            self.ids.fetch_max(
                record
                    .status_watermark()
                    .checked_add(1)
                    .ok_or(NativeError::IdExhausted)?,
                Ordering::Relaxed,
            );
        }
        let id = self
            .ids
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| NativeError::IdExhausted)?;
        MacRepair::reserve_status(&io, &self.env.inventory, id, deadline)?;
        // Allocate once from the adapter counter; the generic reader consumes this one reserved id.
        read_status(&self.env, &AtomicU64::new(id), deadline)
    }
}

fn unreadable_record_text() -> String {
    "The earlier repair record can't be read safely. Nothing was retried; removal and reinstall \
     remain available."
        .to_owned()
}

fn admission_text(error: NativeError) -> String {
    match error {
        NativeError::Invalid
        | NativeError::Foreign
        | NativeError::Unsupported
        | NativeError::Oversize => {
            "The Crosspane files next to this installer don't match what this build approved, or \
             this Mac can't install them. Nothing was changed."
                .to_owned()
        }
        _ => "Crosspane's install can't be read just now. Nothing was changed.".to_owned(),
    }
}

/// Why nothing was changed, with the coordinator's own next step.
fn guidance_text(error: NativeError) -> String {
    let guidance = plan_guidance(error);
    let lead = match guidance {
        RepairGuidance::EnableManually => {
            "Crosspane's sign-in item is turned off, and repair never turns it back on."
        }
        RepairGuidance::RestoreOrRemoveFileOrUninstallThenInstall => {
            "Something where Crosspane installs wasn't put there by Crosspane, or no longer \
             matches what setup created."
        }
        RepairGuidance::UninstallThenInstall => {
            "Repair needs the Crosspane that setup installed to be running, with its files as \
             setup left them. It isn't, or they aren't."
        }
        RepairGuidance::ReDetect => "Crosspane's install couldn't be read just now.",
    };
    format!("{lead} {} Nothing was changed.", guidance.message())
}

/// The person-readable preview of the typed full-reinstall effects.
fn render_repair(preview: &RepairPreview) -> String {
    let activity = match &preview.activity {
        Some(a) => {
            let mut busy = Vec::new();
            if a.input_active {
                busy.push("shared keyboard and mouse".to_owned());
            }
            if a.projections > 0 {
                busy.push(format!("{} shared window(s)", a.projections));
            }
            if a.audio_peers > 0 {
                busy.push("shared sound".to_owned());
            }
            if busy.is_empty() {
                "nothing was in use when this was checked".to_owned()
            } else {
                format!("this ends: {}", busy.join(", "))
            }
        }
        None => "whether anything is in use couldn't be checked, so anything in progress may end"
            .to_owned(),
    };
    format!(
        "Repair puts back Crosspane's app, its command-line tool and its sign-in item as this \
         installer ships them. Crosspane is stopped, then started again; {activity}. Your \
         settings, identity, pairings and permissions are kept, and files Crosspane didn't \
         create are left alone. The previous copy is kept until the new Crosspane reports \
         healthy."
    )
}

fn unfinished_text() -> String {
    "A repair is still unfinished. Resume checks it before anything is retried.".to_owned()
}

fn unknown_finish(detail: &str, resumable: bool) -> RepairFinish {
    RepairFinish {
        outcome: Shown::OutcomeUnknown,
        lines: vec![
            detail.to_owned(),
            "Nothing was retried. Closing and reopening setup checks the install again, and \
             removing Crosspane then installing it again is always available."
                .to_owned(),
        ],
        resumable,
    }
}

/// A repair that was dispatched and can't be finished: the previous copy is kept.
fn retained_finish(pending: &PendingRepair) -> RepairFinish {
    let mut lines = vec![
        "The repair stopped before it could finish. Nothing already settled was repeated."
            .to_owned(),
    ];
    if let Some(error) = pending.error() {
        lines.push(
            match error {
                NativeError::Timeout | NativeError::Cancelled => "A step ran out of time.",
                NativeError::OutcomeUnknown => "A step may have completed, so it was not repeated.",
                NativeError::Refused | NativeError::Foreign => {
                    "What was checked changed before it could be used."
                }
                _ => "A step couldn't be completed.",
            }
            .to_owned(),
        );
    }
    lines.push("The previous copy of Crosspane is kept so nothing is lost.".to_owned());
    if let Some(path) = pending.retained_prior() {
        lines.push(format!("Kept: {}", path.display()));
    }
    RepairFinish {
        outcome: Shown::RecoveryRetained,
        lines,
        resumable: false,
    }
}

/// The old agent was booted out and its clean exit couldn't be settled: nothing was replaced,
/// and Crosspane may be down.
fn stopped_retained(pending: &PendingRepair) -> RepairFinish {
    let mut finish = retained_finish(pending);
    finish.lines.insert(
        1,
        "Crosspane was stopped and may not be running. Start it again from your Applications \
         folder, or sign out and in again."
            .to_owned(),
    );
    finish
}

fn health_finish(progress: RepairProgress, pending: &PendingRepair) -> RepairFinish {
    match progress {
        RepairProgress::Verified => RepairFinish {
            outcome: Shown::Verified,
            lines: vec![
                "The new Crosspane reported healthy, and the previous copy was retired.".to_owned(),
                "Crosspane's app, command-line tool and sign-in item were put back.".to_owned(),
            ],
            resumable: false,
        },
        RepairProgress::HealthVerifiedCleanupIncomplete => {
            let mut lines = vec![
                "The new Crosspane reported healthy. Some cleanup of the previous copy is still \
                 left; it does no harm."
                    .to_owned(),
            ];
            if let Some(path) = pending.retained_prior() {
                lines.push(format!("Kept: {}", path.display()));
            }
            RepairFinish {
                outcome: Shown::HealthVerifiedCleanupIncomplete,
                lines,
                resumable: false,
            }
        }
        _ => retained_finish(pending),
    }
}

impl NativeRepairer {
    /// A Mac repair is never closeable while it waits. The private record lets a new window
    /// reassess, but only this window's genuine pending token can verify the new payload, so a
    /// closed wait would end as "remove, then install" even when the new instance is healthy.
    fn waiting(&self, detail: &str, if_timed_out: RepairFinish) -> RepairStep {
        RepairStep::Waiting {
            detail: detail.to_owned(),
            if_timed_out,
            closeable: false,
        }
    }

    /// The old agent was booted out and hasn't been seen to exit cleanly: nothing was replaced.
    fn wait_for_clean_stop(&self, detail: &str) -> RepairStep {
        let mut if_timed_out = unknown_finish(
            "The old Crosspane didn't exit cleanly in time, so nothing was replaced.",
            true,
        );
        if_timed_out.lines.insert(
            1,
            "Crosspane was stopped and may not be running. Start it again from your \
             Applications folder, or sign out and in again."
                .to_owned(),
        );
        self.waiting(detail, if_timed_out)
    }

    fn wait_for_health(&self) -> RepairStep {
        self.waiting(
            "Crosspane was started again. Waiting for the new instance to report healthy…",
            unknown_finish(
                "The new Crosspane didn't report healthy in time, so what the repair did can't be \
                 proved.",
                true,
            ),
        )
    }
}

impl Repairer for NativeRepairer {
    fn inspect(&mut self, deadline: &Deadline) -> RepairOffer {
        self.kept = None;
        if self.running.is_some() {
            return RepairOffer {
                repair: Availability::Unavailable(unfinished_text()),
                resumable: Some(vec![
                    "A repair started the new Crosspane and is waiting for it to report healthy."
                        .to_owned(),
                ]),
            };
        }
        let unavailable = |text: String| RepairOffer {
            repair: Availability::Unavailable(text),
            resumable: None,
        };
        let mut repair = match self.admit(deadline) {
            Ok(repair) => repair,
            Err(text) => return unavailable(text),
        };
        // Discovery is read-only: a saved boundary is an offer to reassess, never a replay plan.
        match self.env.io().and_then(|io| MacRepair::saved_record(&io, &self.env.inventory, deadline)) {
            Ok(Some(_)) => return RepairOffer {
                repair: Availability::Unavailable(unfinished_text()),
                resumable: Some(vec!["Its private repair record was kept on this Mac. Here Resume only checks the installed files and the running Crosspane; it never continues the earlier repair's steps.".to_owned()]),
            },
            Err(_) => return unavailable(unreadable_record_text()),
            Ok(None) => {}
        }
        // Repair needs the running agent: it is read now, and judged by the coordinator's plan.
        let current = read_status(&self.env, &self.ids, deadline).ok();
        let current_ref = current.as_ref().map(|(agent, reply)| (agent, reply));
        match repair.plan(1, 1, current_ref, deadline) {
            Ok(_) => RepairOffer {
                repair: Availability::Available,
                resumable: None,
            },
            Err(error) => unavailable(guidance_text(error)),
        }
    }

    fn plan(
        &mut self,
        status: Option<&AgentReply>,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<String, String> {
        self.kept = None;
        if self.running.is_some() {
            return Err(unfinished_text());
        }
        let mut repair = self.admit(deadline)?;
        self.no_saved_record(deadline)?;
        let current = self.current(status, deadline);
        let current_ref = current.as_ref().map(|(agent, reply)| (agent, reply));
        let plan = repair
            .plan(operation.0, operation.0, current_ref, deadline)
            .map_err(guidance_text)?;
        let preview = plan.preview();
        let text = render_repair(preview);
        self.kept = Some(KeptRepair {
            operation,
            effects: preview.effects.clone(),
            activity: preview.activity.clone(),
        });
        Ok(text)
    }

    fn confirm(
        &mut self,
        status: Option<&AgentReply>,
        plan: OperationId,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<RepairStep, String> {
        let kept = self
            .kept
            .take()
            .filter(|k| k.operation == plan)
            .ok_or_else(|| {
                "That repair preview is no longer current. Review the repair again. Nothing was \
                 changed."
                    .to_owned()
            })?;
        if self.running.is_some() {
            return Err(unfinished_text());
        }
        // A preview's evidence lives seconds and a person takes longer to read it, so everything
        // is observed again now and the repair starts only if it still previews what was shown.
        let mut repair = self.admit(deadline)?;
        self.no_saved_record(deadline)?;
        let not_answering = || {
            "Crosspane's running agent didn't answer just now, so the repair can't start. Nothing \
             was changed. Review the repair again."
                .to_owned()
        };
        let (selected, reply) = self.current(status, deadline).ok_or_else(not_answering)?;
        let fresh = repair
            .plan(
                operation.0,
                operation.0,
                Some((&selected, &reply)),
                deadline,
            )
            .map_err(guidance_text)?;
        let preview = fresh.preview();
        if preview.effects != kept.effects
            || (kept.activity.is_some() && preview.activity != kept.activity)
        {
            return Err(
                "Things changed since the preview was shown. Nothing was changed; review the \
                 repair again."
                    .to_owned(),
            );
        }
        let consent = fresh
            .consent(operation.0, operation.0, true)
            .map_err(guidance_text)?;
        // The apply takes a Status newer than the one the plan used: read the next one now.
        let (selected, reply) = self.read_current(deadline).map_err(|_| not_answering())?;
        let mut pending = match repair.apply(fresh, consent, Some((&selected, &reply)), deadline) {
            Ok(pending) => pending,
            // A call that ran out of time may have dispatched: it is never assumed unchanged.
            Err(NativeError::Timeout | NativeError::Cancelled | NativeError::OutcomeUnknown) => {
                // A surviving private record can be reassessed by Resume (never replayed).
                let saved = self.no_saved_record(deadline).is_err();
                return Ok(RepairStep::Finished(unknown_finish(
                    "The repair was cut short, and what it did can't be proved.",
                    saved,
                )));
            }
            // Stale or foreign events are refused before any probe, command or file change.
            Err(_) => {
                return Err("Things changed since the preview was shown. Nothing was \
                                changed; review the repair again."
                    .to_owned());
            }
        };
        // The old agent stops first; wait for that within this same bounded stage.
        let mut stop_failed = false;
        while pending.progress() == RepairProgress::WaitingForCleanStop {
            if deadline.check().is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(250));
            if repair.continue_clean_stop(&mut pending, deadline).is_err() {
                stop_failed = true;
                break;
            }
        }
        match pending.progress() {
            RepairProgress::WaitingForCleanStop => {
                let step = self.wait_for_clean_stop(
                    "Crosspane was stopped. Waiting for it to exit cleanly before any file is \
                     replaced…",
                );
                self.running = Some(ActiveRepair { repair, pending });
                Ok(step)
            }
            RepairProgress::Published => {
                // The boundary is only a hint; the genuine pending token continues either way.
                let _ = repair.health_wait(&pending, deadline);
                let step = self.wait_for_health();
                self.running = Some(ActiveRepair { repair, pending });
                Ok(step)
            }
            _ if stop_failed => Ok(RepairStep::Finished(stopped_retained(&pending))),
            progress => Ok(RepairStep::Finished(health_finish(progress, &pending))),
        }
    }

    fn verify(&mut self, _status: Option<&AgentReply>, deadline: &Deadline) -> RepairStep {
        let Some(mut active) = self.running.take() else {
            return RepairStep::Finished(unknown_finish(
                "No repair is running in this window any more.",
                false,
            ));
        };
        if active.pending.progress() == RepairProgress::WaitingForCleanStop {
            if active
                .repair
                .continue_clean_stop(&mut active.pending, deadline)
                .is_err()
            {
                return RepairStep::Finished(stopped_retained(&active.pending));
            }
            if active.pending.progress() == RepairProgress::WaitingForCleanStop {
                let step = self.wait_for_clean_stop(
                    "Waiting for the stopped Crosspane to exit cleanly before any file is \
                     replaced…",
                );
                self.running = Some(active);
                return step;
            }
        }
        if active.pending.progress() != RepairProgress::Published {
            let progress = active.pending.progress();
            return RepairStep::Finished(health_finish(progress, &active.pending));
        }
        // The new instance is judged from a Status read now, newer than every earlier receipt.
        // The controller's own call ids sit below the coordinator's watermark, so they are not used.
        let looked = self.read_current(deadline).ok().and_then(|(agent, reply)| {
            active
                .repair
                .expect_health(&mut active.pending, reply.id)
                .ok()?;
            Some((agent, reply))
        });
        let Some((selected, reply)) = looked else {
            let step = self.wait_for_health();
            self.running = Some(active);
            return step;
        };
        match active
            .repair
            .observe(&mut active.pending, &selected, reply, deadline)
        {
            Ok(completion) => {
                RepairStep::Finished(health_finish(completion.progress, &active.pending))
            }
            // An old instance still answering, or a Status that isn't health yet: look again.
            Err(_) => {
                let step = self.wait_for_health();
                self.running = Some(active);
                step
            }
        }
    }

    fn resume(
        &mut self,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<RepairFinish, String> {
        if self.running.is_none() {
            let mut repair = self.admit(deadline)?;
            let io = self.env.io().map_err(admission_text)?;
            if MacRepair::saved_record(&io, &self.env.inventory, deadline)
                .map_err(admission_text)?
                .is_none()
            {
                return Err(
                    "There is no earlier repair record to reassess. Nothing was changed."
                        .to_owned(),
                );
            }
            let current = self.read_current(deadline).ok();
            let io = self.env.io().map_err(admission_text)?;
            let record = MacRepair::saved_record(&io, &self.env.inventory, deadline)
                .map_err(admission_text)?
                .ok_or_else(unfinished_text)?;
            let result = repair.resume(record, current.as_ref().map(|(agent, reply)| (agent, reply)), deadline).map_err(|_| "The earlier repair couldn't be assessed safely. Recovery material was kept; nothing was retried.".to_owned())?;
            return Ok(match result {
                RepairReassessment::CurrentInstallHealthy => RepairFinish {
                    outcome: Shown::CheckedAfterEarlierRepair,
                    lines: vec!["The installed files match verified receipts and the running Crosspane reports healthy. The earlier repair record was cleared after these checks.".to_owned()],
                    resumable: false,
                },
                RepairReassessment::CurrentInstallHealthyCleanupIncomplete => RepairFinish {
                    outcome: Shown::CheckedAfterEarlierRepair,
                    lines: vec!["The installed files match verified receipts and the running Crosspane reports healthy, but the earlier repair record couldn't be cleared. No repair step was repeated.".to_owned()],
                    resumable: false,
                },
                RepairReassessment::AgentStopped => RepairFinish {
                    outcome: Shown::RecoveryRetained,
                    lines: vec!["Crosspane's installed files still match their verified receipts, but Crosspane didn't answer and its launch agent isn't loaded. Start Crosspane from Applications, or sign out and in, to run it again. The earlier repair can't be confirmed from here: to clear it, remove Crosspane (keeping identity by default) and install it again. The repair record and recovery files were kept.".to_owned()],
                    resumable: true,
                },
                RepairReassessment::RecoveryRetained => RepairFinish {
                    outcome: Shown::RecoveryRetained,
                    lines: vec!["Crosspane couldn't be verified as installed and healthy after the earlier repair. Nothing was retried; recovery files and the repair record were kept. Remove Crosspane, keeping identity by default, then install it again.".to_owned()],
                    resumable: true,
                },
            });
        }
        Ok(match self.verify(status, deadline) {
            RepairStep::Finished(finish) => finish,
            RepairStep::Waiting { .. } => unknown_finish(
                "The new Crosspane hasn't reported healthy yet, and nothing was repeated.",
                true,
            ),
        })
    }
}

// ---- practice fixture -------------------------------------------------------------------------

pub struct NativeFixtures {
    env: Arc<NativeEnv>,
}

impl FixtureLauncher for NativeFixtures {
    fn launch(
        &mut self,
        font: &std::path::Path,
        deadline: &Deadline,
    ) -> Result<FixtureChild, String> {
        let attempt = (|| {
            let io = self.env.io()?;
            let agent = self.env.agent_requirement()?;
            let main = io.admit_main_signature(&io.target().agent_path(), &agent, deadline)?;
            let support = io.admit_support(&main, deadline)?;
            let tutorial = self
                .env
                .rule("Crosspane.app/Contents/MacOS/crosspane-tutorial")?;
            let signature = io.admit_artifact_signature(
                &io.target()
                    .app_path()
                    .join("Contents/MacOS/crosspane-tutorial"),
                &tutorial,
                &main,
                deadline,
            )?;
            io.launch_tutorial(&support, &signature, font, deadline)
        })();
        attempt
            .map(|child| FixtureChild {
                inner: FixtureChildInner::Native(Box::new(child)),
            })
            .map_err(|_| "The practice window couldn't be started.".to_owned())
    }
}

impl std::fmt::Debug for MacProbes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MacProbes")
    }
}

impl std::fmt::Debug for NativeEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeEnv")
    }
}

#[cfg(test)]
mod repair_wording_tests {
    use super::*;

    #[test]
    fn every_refusal_carries_the_coordinators_own_next_step_and_says_nothing_changed() {
        for (error, tail) in [
            (
                NativeError::Unsupported,
                "Enable the agent manually before repair",
            ),
            (
                NativeError::Foreign,
                "Restore or remove the file, or uninstall then install.",
            ),
            (
                NativeError::Refused,
                "Uninstall (keeping identity by default), then install.",
            ),
            (
                NativeError::Timeout,
                "Inspect the retained installation and obtain fresh observations.",
            ),
        ] {
            let text = guidance_text(error);
            assert!(text.contains(tail), "{error:?}: {text}");
            assert!(text.ends_with("Nothing was changed."), "{error:?}: {text}");
        }
    }

    #[test]
    fn the_preview_names_the_interruption_and_fits_the_view() {
        let effects = vec![RepairEffect::BootstrapSelectedAgent];
        let idle = render_repair(&RepairPreview {
            effects: effects.clone(),
            activity: Some(RepairActivity {
                input_active: false,
                projections: 0,
                audio_peers: 0,
            }),
        });
        assert!(idle.contains("nothing was in use"), "{idle}");
        let busy = render_repair(&RepairPreview {
            effects: effects.clone(),
            activity: Some(RepairActivity {
                input_active: true,
                projections: 2,
                audio_peers: 1,
            }),
        });
        assert!(
            busy.contains("this ends: shared keyboard and mouse, 2 shared window(s), shared sound"),
            "{busy}"
        );
        let blind = render_repair(&RepairPreview {
            effects,
            activity: None,
        });
        assert!(blind.contains("couldn't be checked"), "{blind}");
        for text in [idle, busy, blind] {
            assert!(text.len() <= 600, "{} bytes: {text}", text.len());
            assert!(text.contains("stopped, then started again"));
            assert!(text.contains("pairings and permissions are kept"));
        }
    }
}
