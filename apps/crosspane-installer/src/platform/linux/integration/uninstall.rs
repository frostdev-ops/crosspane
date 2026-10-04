//! Consented, ordered removal on the merged Linux uninstall coordinator.
//!
//! The coordinator's types are opaque and single use, so this adapter only sequences them:
//! inventory the installed files from the verified install record, plan with the person's
//! choices, hold the plan for the preview, and after the person's explicit confirmation begin the
//! run and advance it one stage at a time. Each stage gets its own bounded deadline (the firewall
//! stage up to the frozen 120 s), a later stage gets a fresh deadline only after the earlier one
//! finished, and an expired call is never extended. Results are reported exactly as the
//! coordinator typed them: a result that can't be proved clean says so and keeps recovery tools.

use std::sync::Arc;

use crosspane_installer_core::OperationId;

use super::super::firewall::{
    FirewallError, LinuxFirewall, ManagerSelection, RuleKind, receipts::DurableIntentStore,
};
use super::super::native_io::{
    Cancellation, ChildEnvironment, Deadline, LinuxNativeIo, MAX_NATIVE_TIMEOUT_MS, NativeError,
    SupportProof,
};
use super::super::payload::{Package, PayloadInstaller};
use super::super::removal::executor::{
    CleanupInventory, CleanupPlanner, CleanupResult, UninstallConsent, UninstallFirewall,
    UninstallForm, UninstallPlan, UninstallPlanner, UninstallReport, UninstallRulePlan,
    UninstallRun, UninstallStage,
};
use super::super::removal::{IdentityChoice, RemovalSelection, TrackedAgent};
use super::super::service::LinuxService;
use super::super::transport::CallerClock;
use super::domains::{Support, SupportOutcome, UninstallOffer, UninstallProgress, Uninstaller};
use super::resume;
use crate::live::{Availability, MaintenanceOutcome, RemovalChoice};
use crate::view::ToggleRole;

const READ_MS: u64 = 15_000;
const STAGE_MS: u64 = 30_000;
const IDENTITY_CHOICE: u16 = 1;
const LAN_CHOICE: u16 = 2;
/// The one separately consented follow-up this binding ever raises.
const LAN_FOLLOW_UP: u16 = 1;

pub struct NativeUninstaller {
    io: Arc<LinuxNativeIo>,
    env: ChildEnvironment,
    #[allow(dead_code)]
    clock: CallerClock,
    support: Arc<dyn Support>,
    state: State,
    /// Rule choices only appear when the installer's own receipt for the rule can be read.
    lan_receipt: bool,
    /// Rule operations must exceed the cleanup operation and never repeat in the journal.
    next: u64,
}

enum State {
    Idle,
    Planned(Box<Planned>),
    Running(Box<Running>),
}

struct Planned {
    plan: UninstallPlan,
    consent: UninstallConsent,
    service: Arc<LinuxService>,
    resume: bool,
    selection: RemovalSelection,
    digest: [u8; 32],
    operation: OperationId,
}

struct Running {
    run: UninstallRun,
    digest: [u8; 32],
    selection: RemovalSelection,
    operation: OperationId,
    firewall: Option<(LinuxFirewall, DurableIntentStore)>,
    pending_rule: Option<(UninstallRulePlan, OperationId)>,
    /// The firewall stage was already attempted. A run still in that stage afterwards could not
    /// settle it (the coordinator retired it or refused to keep the rule), so it is finished
    /// with its honest report instead of being attempted again forever.
    firewall_tried: bool,
}

fn stage_deadline(ms: u64) -> Option<Deadline> {
    Deadline::new(ms.clamp(1, MAX_NATIVE_TIMEOUT_MS), Cancellation::default()).ok()
}

impl NativeUninstaller {
    pub fn new(
        io: Arc<LinuxNativeIo>,
        env: ChildEnvironment,
        clock: CallerClock,
        support: Arc<dyn Support>,
    ) -> Self {
        Self {
            io,
            env,
            clock,
            support,
            state: State::Idle,
            lan_receipt: false,
            next: 0,
        }
    }

    fn proof(&self, package: Option<&Package>) -> Option<SupportProof> {
        let deadline = stage_deadline(READ_MS)?;
        match self.support.detect(package, &deadline) {
            SupportOutcome::Supported(proof) => Some(proof),
            _ => None,
        }
    }

    fn service(&self, package: &Package, deadline: &Deadline) -> Result<Arc<LinuxService>, String> {
        let installer = PayloadInstaller::new(self.io.clone())
            .map_err(|_| "The install locations can't be admitted.".to_owned())?;
        let resources = installer
            .rendered_resources(package)
            .map_err(|_| "The staged payload doesn't describe this install.".to_owned())?;
        LinuxService::new(
            self.io.clone(),
            super::session_of(&self.env),
            resources,
            deadline,
        )
        .map(Arc::new)
        .map_err(|_| "Crosspane's service can't be read just now.".to_owned())
    }

    fn agent_digest(package: &Package) -> Option<[u8; 32]> {
        package
            .manifest()
            .members
            .iter()
            .find(|m| m.name == "bin/crosspane-agent")
            .and_then(|m| super::hex_hash(&m.sha256).ok())
    }

    /// Prepare the firewall rule step: only a rule the person chose and the installer's own
    /// admitted receipt covers is ever planned. Anything missing keeps the rule, said plainly.
    fn prepare_rule(&mut self, package: Option<&Package>) -> UninstallProgress {
        let proof = self.proof(package);
        let rule_op = {
            self.next += 1;
            self.next
        };
        let State::Running(running) = &mut self.state else {
            return finished_empty();
        };
        let Some(d) = stage_deadline(READ_MS) else {
            return finish(&mut self.state, "Removal stopped early.");
        };
        let keep = |running: &mut Running, why: FirewallError, text: &str| {
            let _ = running.run.retain_rule(RuleKind::Lan, why, &d);
            UninstallProgress::Progress(text.to_owned())
        };
        let Some(proof) = proof else {
            return keep(
                running,
                FirewallError::Native(NativeError::Unsupported),
                "Support couldn't be proved right now, so the firewall rule was kept.",
            );
        };
        let Some(bytes) = resume::read_receipt(&self.io) else {
            return keep(
                running,
                FirewallError::Kept,
                "Crosspane's own record of the firewall rule couldn't be read, so the rule was kept.",
            );
        };
        let mut firewall = LinuxFirewall::new(self.io.clone());
        let store = match DurableIntentStore::open(&mut firewall, &proof) {
            Ok(store) => store,
            Err(error) => {
                return keep(
                    running,
                    FirewallError::Native(error),
                    "The firewall journal couldn't be opened, so the rule was kept.",
                );
            }
        };
        let receipt = match store.admit_receipt(&proof, &bytes) {
            Ok(receipt) => receipt,
            Err(error) => {
                return keep(
                    running,
                    FirewallError::Native(error),
                    "The firewall record doesn't match the journal, so the rule was kept.",
                );
            }
        };
        let (mut firewall, mut store) = (firewall, store);
        let operation = OperationId(running.operation.0.saturating_add(1 + rule_op));
        let planned = {
            let mut context = UninstallFirewall {
                firewall: &mut firewall,
                support: &proof,
                store: &mut store,
                manager: ManagerSelection::Ufw,
            };
            running
                .run
                .prepare_rule(&mut context, RuleKind::Lan, receipt, operation, &d)
        };
        match planned {
            Ok(plan) => {
                let preview = plan.preview();
                running.firewall = Some((firewall, store));
                running.pending_rule = Some((plan, operation));
                UninstallProgress::FollowUp {
                    id: LAN_FOLLOW_UP,
                    label: "Remove the firewall rule".into(),
                    preview,
                }
            }
            // The coordinator already recorded why the rule was kept and moved on.
            Err(_) => UninstallProgress::Progress(
                "The firewall rule can't be removed safely right now, so it was kept.".into(),
            ),
        }
    }
}

fn selection(choices: &[(u16, bool)], lan_available: bool) -> RemovalSelection {
    let checked = |id| choices.iter().any(|(c, on)| *c == id && *on);
    RemovalSelection {
        identity: if checked(IDENTITY_CHOICE) {
            IdentityChoice::DeleteIdentityAndPairings
        } else {
            IdentityChoice::Keep
        },
        lan_rule: lan_available && checked(LAN_CHOICE),
        mdns_rule: false,
    }
}

fn finished_empty() -> UninstallProgress {
    UninstallProgress::Finished(
        MaintenanceOutcome::Refused,
        vec!["No removal is running.".into()],
    )
}

impl Uninstaller for NativeUninstaller {
    fn inspect(&mut self, package: Option<&Package>, _deadline: &Deadline) -> UninstallOffer {
        self.state = State::Idle;
        let unavailable = |text: &str| UninstallOffer {
            uninstall: Availability::Unavailable(text.into()),
            choices: Vec::new(),
        };
        if package.is_none() {
            return unavailable(
                "Removal needs the staged payload (--payload) that matches the installed \
                 version, to check the installed files before deleting any.",
            );
        }
        let Some(deadline) = stage_deadline(READ_MS) else {
            return unavailable("Removal can't start right now.");
        };
        if self.io.admit_cleanup(&deadline).is_err() {
            return unavailable(
                "Crosspane's install record can't be read or doesn't match what is on disk, so \
                 nothing will be removed. Files setup didn't create are never deleted.",
            );
        }
        self.lan_receipt = resume::read_receipt(&self.io).is_some();
        let mut choices = vec![RemovalChoice {
            id: IDENTITY_CHOICE,
            role: ToggleRole::DeleteIdentity,
            label: "Also erase this computer's identity and pairings (it will need to be paired \
                    again)"
                .into(),
            checked: false,
            enabled: true,
        }];
        if self.lan_receipt {
            choices.push(RemovalChoice {
                id: LAN_CHOICE,
                role: ToggleRole::Grant,
                label: "Also remove the firewall rule Crosspane added (it affects every user of \
                        this computer, and asks for your approval separately)"
                    .into(),
                checked: false,
                enabled: true,
            });
        }
        UninstallOffer {
            uninstall: Availability::Available,
            choices,
        }
    }

    fn plan(
        &mut self,
        package: Option<&Package>,
        choices: &[(u16, bool)],
        operation: OperationId,
        _deadline: &Deadline,
    ) -> Result<String, String> {
        self.state = State::Idle;
        let package = package.ok_or_else(|| "Removal needs the staged payload.".to_owned())?;
        let digest = Self::agent_digest(package)
            .ok_or_else(|| "The staged payload doesn't list the agent.".to_owned())?;
        let deadline =
            stage_deadline(READ_MS).ok_or_else(|| "Removal can't start right now.".to_owned())?;
        let selection = selection(choices, self.lan_receipt);
        let revision = operation.0;
        let proof = self.io.admit_cleanup(&deadline).map_err(|_| {
            "The install record can't be read, so nothing will be removed.".to_owned()
        })?;
        let inventory = CleanupInventory::admit(proof, &deadline)
            .map_err(|_| "The installed files can't be matched to the record.".to_owned())?;
        let cleanup = CleanupPlanner::default();
        let cleanup_plan = cleanup
            .plan(inventory, revision, operation, selection)
            .map_err(|_| "The removal plan is out of date. Review it again.".to_owned())?;
        let cleanup_consent = cleanup
            .consent(&cleanup_plan, revision, operation, &deadline)
            .map_err(|_| "The installed files changed while planning. Review again.".to_owned())?;
        // An unfinished removal is resumed, never begun a second time over its record.
        let resume = cleanup_plan
            .read_intent(&cleanup_consent, &deadline)
            .map_err(|_| {
                "An earlier removal record doesn't match, so nothing will be removed.".to_owned()
            })?
            .is_some();
        // The running agent, if any, is bound as the original: only its clean exit can ever
        // unlock identity erase or the removal of recovery tools.
        let original = TrackedAgent::capture(self.io.clone(), &deadline)
            .ok()
            .map(Arc::new);
        let planner = UninstallPlanner::default();
        let plan = planner
            .plan(cleanup_plan, self.io.clone(), original)
            .map_err(|_| {
                "The running agent doesn't match this install. Nothing will be removed.".to_owned()
            })?;
        let consent = planner
            .consent(&plan, cleanup_consent, revision, operation)
            .map_err(|_| "The removal plan is out of date. Review it again.".to_owned())?;
        let service = self.service(package, &deadline)?;
        let form = match plan.form() {
            UninstallForm::AwaitingOriginalCleanExit => {
                "Crosspane is running, so it is stopped first and its clean exit is checked."
            }
            _ => {
                "No running Crosspane was matched, so identity and recovery tools are kept: a \
                 clean exit can't be proved."
            }
        };
        let preview = format!(
            "{}\n\n{form}{}",
            plan.preview(),
            if resume {
                "\n\nAn earlier removal didn't finish. It is resumed from its record after you \
                 confirm, and nothing already settled is repeated."
            } else {
                ""
            }
        );
        self.state = State::Planned(Box::new(Planned {
            plan,
            consent,
            service,
            resume,
            selection,
            digest,
            operation,
        }));
        Ok(preview)
    }

    fn begin(&mut self, _package: Option<&Package>, _operation: OperationId) -> Result<(), String> {
        let State::Planned(planned) = std::mem::replace(&mut self.state, State::Idle) else {
            return Err("There is no reviewed removal to start.".into());
        };
        let Planned {
            plan,
            consent,
            service,
            resume,
            selection,
            digest,
            operation,
        } = *planned;
        let deadline =
            stage_deadline(READ_MS).ok_or_else(|| "Removal can't start right now.".to_owned())?;
        let run = if resume {
            plan.resume(consent, service, &deadline)
        } else {
            plan.begin(consent, service, &deadline)
        }
        .map_err(|_| {
            "The removal record couldn't be written, so nothing was removed.".to_owned()
        })?;
        self.state = State::Running(Box::new(Running {
            run,
            digest,
            selection,
            operation,
            firewall: None,
            pending_rule: None,
            firewall_tried: false,
        }));
        Ok(())
    }

    fn advance(&mut self, package: Option<&Package>) -> UninstallProgress {
        let stage = match &self.state {
            State::Running(running) => {
                if running.pending_rule.is_some() {
                    return UninstallProgress::Progress(
                        "Waiting for your answer about the firewall rule.".into(),
                    );
                }
                running.run.stage()
            }
            _ => return finished_empty(),
        };
        if stage == UninstallStage::Firewall {
            if let State::Running(running) = &mut self.state {
                if running.firewall_tried {
                    return finish(
                        &mut self.state,
                        "The firewall step couldn't be settled, so removal stopped there. What is \
                         left is reported below.",
                    );
                }
                running.firewall_tried = true;
            }
            return self.prepare_rule(package);
        }
        let env = self.env.clone();
        let State::Running(running) = &mut self.state else {
            return finished_empty();
        };
        let Some(d) = stage_deadline(STAGE_MS) else {
            return finish(&mut self.state, "Removal stopped early.");
        };
        let result = match stage {
            UninstallStage::Disable => running.run.disable(&d).map(|()| "Startup was turned off."),
            UninstallStage::Stop => running.run.stop(&d).map(|()| "Crosspane was stopped."),
            UninstallStage::Exit => running
                .run
                .observe_exit(&d)
                .map(|()| "Crosspane's exit was checked."),
            UninstallStage::Identity => {
                let digest = running.digest;
                running
                    .run
                    .identity(digest, env, &d)
                    .map(|()| "Identity handling is settled.")
            }
            UninstallStage::Files => running
                .run
                .remove_files(&d)
                .map(|()| "Installed files were handled."),
            UninstallStage::Firewall | UninstallStage::Finished => Ok(""),
        };
        match result {
            Ok(text) => {
                if running.run.stage() == UninstallStage::Finished {
                    finish(&mut self.state, "")
                } else {
                    UninstallProgress::Progress(text.into())
                }
            }
            Err(error) => finish(
                &mut self.state,
                &format!("A stage didn't finish ({error}). What is left is reported below."),
            ),
        }
    }

    fn follow_up(
        &mut self,
        package: Option<&Package>,
        id: u16,
        confirm: bool,
    ) -> UninstallProgress {
        if id != LAN_FOLLOW_UP {
            return UninstallProgress::Progress("That follow-up is no longer waiting.".into());
        }
        // A rule is only removed with a proof taken now, not the one from the preview.
        let proof = if confirm { self.proof(package) } else { None };
        let State::Running(running) = &mut self.state else {
            return finished_empty();
        };
        let Some((plan, operation)) = running.pending_rule.take() else {
            return UninstallProgress::Progress("That follow-up is no longer waiting.".into());
        };
        let revision = running.operation.0;
        let Some(keep_d) = stage_deadline(STAGE_MS) else {
            return finish(&mut self.state, "Removal stopped early.");
        };
        if !confirm {
            let _ = running
                .run
                .retain_rule(RuleKind::Lan, FirewallError::Kept, &keep_d);
            return UninstallProgress::Progress("The firewall rule was kept, as you chose.".into());
        }
        let (Some(proof), Some((firewall, store))) = (proof, running.firewall.as_mut()) else {
            let _ = running
                .run
                .retain_rule(RuleKind::Lan, FirewallError::CurrentRequired, &keep_d);
            return UninstallProgress::Progress(
                "Support couldn't be proved right now, so the firewall rule was kept.".into(),
            );
        };
        let consent = match plan.consent(operation, revision) {
            Ok(consent) => consent,
            Err(_) => {
                let _ = running
                    .run
                    .retain_rule(RuleKind::Lan, FirewallError::Stale, &keep_d);
                return UninstallProgress::Progress(
                    "The rule's preview was out of date, so it was kept.".into(),
                );
            }
        };
        // The system prompt is the person's: this stage may wait as long as the frozen contract
        // allows. It is its own budget, and an expired one is never extended.
        let Some(d) = stage_deadline(MAX_NATIVE_TIMEOUT_MS) else {
            return finish(&mut self.state, "Removal stopped early.");
        };
        let mut context = UninstallFirewall {
            firewall,
            support: &proof,
            store,
            manager: ManagerSelection::Ufw,
        };
        let result = running.run.apply_rule(&mut context, plan, consent, &d);
        match result {
            Ok(()) => UninstallProgress::Progress("The firewall rule step finished.".into()),
            Err(_) => UninstallProgress::Progress(
                "The firewall rule couldn't be confirmed removed, so it is reported as unsettled."
                    .into(),
            ),
        }
    }
}

fn finish(state: &mut State, note: &str) -> UninstallProgress {
    let State::Running(running) = std::mem::replace(state, State::Idle) else {
        return finished_empty();
    };
    let report = running.run.report();
    let (outcome, mut lines) = describe(&report, running.selection);
    if !note.is_empty() {
        lines.insert(0, note.to_owned());
    }
    UninstallProgress::Finished(outcome, lines)
}

fn describe(
    report: &UninstallReport,
    selection: RemovalSelection,
) -> (MaintenanceOutcome, Vec<String>) {
    let p = &report.progress;
    let done =
        |r: CleanupResult| matches!(r, CleanupResult::Removed | CleanupResult::AlreadyAbsent);
    let mut lines = Vec::new();
    lines.push(
        if done(p.autostart) {
            "Crosspane no longer starts when you sign in."
        } else {
            "Crosspane may still start when you sign in: turning that off wasn't confirmed."
        }
        .to_owned(),
    );
    lines.push(
        if done(p.stop) {
            "Crosspane was stopped and left cleanly."
        } else {
            "A clean stop of the original Crosspane wasn't proved."
        }
        .to_owned(),
    );
    lines.push(
        match (selection.identity, p.identity) {
            (IdentityChoice::Keep, _) => "This computer's identity and pairings were kept.",
            (_, CleanupResult::Removed) => {
                "This computer's identity and pairings were erased. The other computer still \
                 trusts it until you remove it there."
            }
            _ => {
                "This computer's identity and pairings were kept: erasing them wasn't proved safe."
            }
        }
        .to_owned(),
    );
    if selection.lan_rule {
        lines.push(
            match p.lan {
                CleanupResult::Removed | CleanupResult::AlreadyAbsent => {
                    "The firewall rule Crosspane added was removed."
                }
                CleanupResult::Unknown | CleanupResult::Failed => {
                    "Whether the firewall rule was removed couldn't be confirmed."
                }
                _ => "The firewall rule was kept.",
            }
            .to_owned(),
        );
    }
    let removed = p.resources.iter().filter(|r| done(**r)).count();
    let unsure = p
        .resources
        .iter()
        .filter(|r| matches!(r, CleanupResult::Unknown | CleanupResult::Failed))
        .count();
    lines.push(format!(
        "{removed} of {} installed files were removed.",
        p.resources.len()
    ));
    if unsure > 0 {
        lines.push(format!(
            "{unsure} file(s) couldn't be confirmed removed; they were not retried."
        ));
    }
    if report.recovery_retained {
        lines.push(
            "Recovery tools were kept, because a clean exit of the original agent wasn't proved."
                .into(),
        );
    }
    if report.empty_directories_retained {
        lines.push("Some empty folders were left behind.".into());
    }
    if !report.issues.is_empty() {
        lines.push(format!(
            "{} step(s) reported a problem; nothing uncertain was retried.",
            report.issues.len()
        ));
    }
    let outcome = if report.form == UninstallForm::Complete {
        MaintenanceOutcome::Removed
    } else if report.stage == UninstallStage::Finished {
        MaintenanceOutcome::Partial
    } else {
        MaintenanceOutcome::Failed
    };
    (outcome, lines)
}

opaque_debug!(NativeUninstaller);
