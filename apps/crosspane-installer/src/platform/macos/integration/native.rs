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
use super::super::transport::{CallerClock, MacAgentPort, SelectedAgent, SelectedLink};
use super::domains::{
    Admitted, AdmittedInner, Agents, AudioError, AudioPackages, AudioPreview, AudioState,
    DomainFactory, Domains, FixtureChild, FixtureChildInner, FixtureLauncher, InstallApplied,
    InstallError, InstallPreview, InstallState, Installs, Support, SupportOutcome, UninstallOffer,
    UninstallResult, Uninstaller,
};
use crate::agent_contract::{
    AgentCall, AgentPort, AgentReply, DecodedReply, InstallerRequest, StatusAdmission,
};
use crate::live::{Availability, MaintenanceOutcome, RemovalChoice};
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
        let selected = admit_selected(&self.env, None, deadline)?;
        let mut port = MacAgentPort::new(selected.clone(), self.env.caller_clock())?;
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
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
