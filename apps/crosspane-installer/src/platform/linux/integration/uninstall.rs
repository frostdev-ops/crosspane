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

use super::super::extension;
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
    /// What taking back the GNOME/KDE desktop files said, once the core files were handled.
    desktop: Vec<String>,
}

/// The state file of the desktop component. Its presence is the only thing that makes removal
/// look at the desktop files at all, so Hyprland removal never touches any of this.
fn desktop_record(io: &LinuxNativeIo) -> std::path::PathBuf {
    io.target()
        .paths()
        .state_home
        .join("crosspane/installer/desktop-outcome.json")
}

/// A sentence for the removal preview when this install added GNOME/KDE desktop files.
fn desktop_preview(io: &LinuxNativeIo) -> Option<&'static str> {
    io.metadata(&desktop_record(io)).ok().flatten().map(|_| {
        "The desktop files Crosspane added for GNOME or KDE (its desktop entry, and on GNOME the \
         Shell extension) are removed too, and the extension is switched off in your GNOME \
         settings. Every other extension stays as it is, and a file you changed is kept."
    })
}

/// Take back the desktop entry and the Shell extension this install added, and turn the
/// extension off in the person's settings. Runs once, after the core files. Nothing here is an
/// error for the removal as a whole: every outcome is a line for the person.
fn remove_desktop(
    io: &Arc<LinuxNativeIo>,
    env: &ChildEnvironment,
    support: &Arc<dyn Support>,
    package: Option<&Package>,
) -> Vec<String> {
    match io.metadata(&desktop_record(io)) {
        Ok(None) => return Vec::new(),
        Ok(Some(_)) => {}
        Err(_) => {
            return vec![
                "Crosspane's desktop files couldn't be checked, so they were kept.".to_owned(),
            ];
        }
    }
    let kept = |why: &str| {
        vec![format!(
            "Crosspane's GNOME/KDE desktop files were kept: {why}."
        )]
    };
    let Some(deadline) = stage_deadline(STAGE_MS) else {
        return kept("removal stopped early");
    };
    let proof = match support.detect(package, &deadline) {
        SupportOutcome::Supported(proof) => proof,
        _ => return kept("support couldn't be proved right now"),
    };
    let Ok(installer) = PayloadInstaller::new(io.clone()) else {
        return kept("the install locations can't be admitted");
    };
    let mut lines = Vec::new();
    let recorded = match installer.desktop_recorded(&proof) {
        Ok(ids) => ids,
        Err(_) => return kept("Crosspane's own record of them can't be read"),
    };
    // The extension's switch goes first; a failure there is a line, and the files still go.
    if recorded
        .iter()
        .any(|id| id.starts_with("resources/gnome-shell-extension/"))
    {
        let switched = extension::GnomeSettings::new(io.clone(), env.values(), &deadline)
            .and_then(|settings| settings.disable(&proof, &deadline));
        lines.push(
            match switched {
                Ok(extension::Change::Written) => {
                    "Crosspane's Shell extension was turned off in your GNOME settings; your \
                     other extensions were not touched."
                }
                Ok(extension::Change::Unchanged) => {
                    "Crosspane's Shell extension was not turned on in your GNOME settings."
                }
                Err(_) => {
                    "Crosspane's Shell extension couldn't be turned off in your GNOME settings; \
                     remove it in the Extensions app."
                }
            }
            .to_owned(),
        );
    }
    match installer.desktop_remove(&proof, &deadline) {
        Ok(report) => {
            lines.push(format!(
                "{} desktop file(s) were removed.",
                report.removed.len()
            ));
            if !report.kept.is_empty() {
                lines.push(format!(
                    "{} desktop file(s) were changed since setup, so they were kept.",
                    report.kept.len()
                ));
            }
        }
        Err(_) => lines.push(
            "Crosspane's desktop files couldn't be removed completely; what is left was not \
             retried."
                .to_owned(),
        ),
    }
    lines
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
            super::manager_session_of(self.env.values()),
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
                 nothing will be removed. Only files setup recorded are ever deleted.",
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
            "{}\n\n{form}{}{}",
            plan.preview(),
            if resume {
                "\n\nAn earlier removal didn't finish. It is resumed from its record after you \
                 confirm, and nothing already settled is repeated."
            } else {
                ""
            },
            desktop_preview(&self.io).map_or(String::new(), |note| format!("\n\n{note}"))
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
            desktop: Vec::new(),
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
        let (io, support, desktop_env) = (self.io.clone(), self.support.clone(), self.env.clone());
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
                    // The core files are handled; the GNOME/KDE desktop files go with them.
                    running.desktop = remove_desktop(&io, &desktop_env, &support, package);
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
    lines.extend(running.desktop);
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
            "Recovery tools were kept, because removing them couldn't be proved safe (the original \
             agent's clean exit, or an earlier repair record, couldn't be confirmed)."
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

#[cfg(test)]
mod desktop_removal_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::platform::linux::detect::Desktop;
    use crate::platform::linux::extension::{ENABLED_KEY, UUID};
    use crate::platform::linux::native_io::{
        CommandOutput, CommandRunner, CommandSpec, ProcessFacts, ProcessProbe, SupportObservations,
    };
    use crate::platform::linux::payload::{DESKTOP_FILES, sha256};
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    static ID: AtomicU64 = AtomicU64::new(0);

    /// `gsettings` on one key, kept in memory.
    #[derive(Default)]
    struct Settings {
        list: Mutex<Vec<String>>,
        fail_set: Mutex<bool>,
        sets: Mutex<usize>,
    }
    impl CommandRunner for Settings {
        fn run(
            &self,
            c: &CommandSpec,
            _: &Deadline,
        ) -> std::result::Result<CommandOutput, NativeError> {
            let argv = c.argv();
            let ok = |text: String| CommandOutput {
                code: Some(0),
                stdout: text.into_bytes(),
                stderr: Vec::new(),
            };
            match (argv[0].as_str(), argv[2].as_str()) {
                ("get", ENABLED_KEY) => {
                    let list = self.list.lock().unwrap();
                    Ok(ok(format!(
                        "[{}]\n",
                        list.iter()
                            .map(|n| format!("'{n}'"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )))
                }
                ("get", "disable-user-extensions") => Ok(ok("false\n".into())),
                ("set", ENABLED_KEY) => {
                    *self.sets.lock().unwrap() += 1;
                    if *self.fail_set.lock().unwrap() {
                        return Ok(CommandOutput {
                            code: Some(1),
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        });
                    }
                    *self.list.lock().unwrap() =
                        crate::platform::linux::extension::parse_enabled(&argv[3]).unwrap();
                    Ok(ok(String::new()))
                }
                other => panic!("unapproved command {other:?}"),
            }
        }
    }
    struct Never;
    impl ProcessProbe for Never {
        fn snapshot(&self, _: u32, _: &Deadline) -> std::result::Result<ProcessFacts, NativeError> {
            Err(NativeError::Unavailable)
        }
    }
    /// Support detection that answers with one fixed proof, or with nothing.
    struct Fixed(Mutex<Option<crate::platform::linux::native_io::SupportProof>>);
    impl Support for Fixed {
        fn detect(&self, _: Option<&Package>, _: &Deadline) -> SupportOutcome {
            match self.0.lock().unwrap().clone() {
                Some(proof) => SupportOutcome::Supported(proof),
                None => SupportOutcome::Pending("not now".into()),
            }
        }
        fn source(&self) -> crosspane_installer_core::ObservationSource {
            crosspane_installer_core::ObservationSource::Demo
        }
    }

    const FILES_AND_PATHS: [(&str, &str); 4] = [
        (
            "applications/io.frostdev.crosspane.agent.desktop",
            "[Desktop Entry]\nType=Application\n",
        ),
        (
            "gnome-shell/extensions/crosspane@frostdev.io/extension.js",
            "// js\n",
        ),
        (
            "gnome-shell/extensions/crosspane@frostdev.io/metadata.json",
            "{\"uuid\": \"crosspane@frostdev.io\"}\n",
        ),
        (
            "gnome-shell/extensions/crosspane@frostdev.io/io.frostdev.Crosspane.Shell1.xml",
            "<node/>\n",
        ),
    ];

    struct World {
        root: PathBuf,
        io: Arc<LinuxNativeIo>,
        env: ChildEnvironment,
        settings: Arc<Settings>,
        support: Arc<dyn Support>,
        _bus: UnixListener,
    }
    impl World {
        /// A GNOME session with the four desktop files and their record in place, and the
        /// extension on among two others.
        fn installed(with_record: bool) -> Self {
            let root = PathBuf::from(format!(
                "/tmp/cpdr-{}-{}",
                std::process::id(),
                ID.fetch_add(1, Ordering::Relaxed)
            ));
            let settings = Arc::new(Settings::default());
            let io =
                Arc::new(LinuxNativeIo::scratch(&root, settings.clone(), Arc::new(Never)).unwrap());
            let proof = io
                .scratch_support(SupportObservations {
                    uid: io.target().paths().uid,
                    desktop: Desktop::Gnome,
                    architecture: std::env::consts::ARCH.into(),
                    arch_based: true,
                    compositor_version: [50, 4, 0],
                    protocols_ready: true,
                    runtime_libraries_ready: true,
                    compositor_managed: true,
                    graphical_target_active: true,
                    graphical_sessions: 1,
                    session_id: "s".into(),
                    session_type: "wayland".into(),
                    seat: "seat0".into(),
                    active: true,
                })
                .unwrap();
            io.create_private_dir(&proof, &io.target().paths().runtime_home)
                .unwrap();
            let state = io.target().paths().state_home.join("crosspane/installer");
            io.create_private_dir(&proof, &state).unwrap();
            let data = io.target().paths().data_home.clone();
            let mut leaves = Vec::new();
            for (index, (relative, text)) in FILES_AND_PATHS.iter().enumerate() {
                let path = data.join(relative);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, text).unwrap();
                std::fs::set_permissions(
                    &path,
                    std::os::unix::fs::PermissionsExt::from_mode(0o644),
                )
                .unwrap();
                let digest: String = sha256(text.as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                leaves.push(serde_json::json!({"id": DESKTOP_FILES[index], "sha256": digest}));
            }
            if with_record {
                let record = serde_json::to_vec(&serde_json::json!({
                    "schema_version": 1, "operation": 1, "desktop": "gnome",
                    "manifest_sha256": "0".repeat(64), "leaves": leaves,
                }))
                .unwrap();
                io.atomic_write(&proof, &state.join("desktop-outcome.json"), &record)
                    .unwrap();
            }
            *settings.list.lock().unwrap() =
                vec!["keep@me.org".into(), UUID.into(), "also@keep.org".into()];
            let bus_path = io.target().paths().runtime_home.join("bus");
            let bus = UnixListener::bind(&bus_path).unwrap();
            let env = io
                .session_bus_environment(
                    BTreeMap::from([(
                        "DBUS_SESSION_BUS_ADDRESS".to_owned(),
                        format!("unix:path={}", bus_path.display()),
                    )]),
                    &Deadline::new(5000, Cancellation::default()).unwrap(),
                )
                .unwrap();
            Self {
                root,
                io,
                env,
                settings,
                support: Arc::new(Fixed(Mutex::new(Some(proof)))),
                _bus: bus,
            }
        }
        fn path(&self, index: usize) -> PathBuf {
            self.io
                .target()
                .paths()
                .data_home
                .join(FILES_AND_PATHS[index].0)
        }
        fn run(&self) -> Vec<String> {
            remove_desktop(&self.io, &self.env, &self.support, None)
        }
    }
    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn removal_turns_only_our_entry_off_and_takes_back_exactly_what_was_installed() {
        let world = World::installed(true);
        assert!(desktop_preview(&world.io).is_some());
        let lines = world.run().join(" ");
        assert!(
            lines.contains("turned off in your GNOME settings"),
            "{lines}"
        );
        assert!(lines.contains("4 desktop file(s) were removed"), "{lines}");
        assert_eq!(
            *world.settings.list.lock().unwrap(),
            ["keep@me.org", "also@keep.org"]
        );
        for index in 0..4 {
            assert!(!world.path(index).exists(), "{index}");
        }
        assert!(
            !world
                .io
                .target()
                .paths()
                .data_home
                .join("gnome-shell/extensions/crosspane@frostdev.io")
                .exists()
        );
        assert!(!desktop_record(&world.io).exists());
        // Nothing left: a second run has nothing to say and touches nothing.
        let sets = *world.settings.sets.lock().unwrap();
        assert!(world.run().is_empty());
        assert_eq!(*world.settings.sets.lock().unwrap(), sets);
        assert!(desktop_preview(&world.io).is_none());
    }

    #[test]
    fn without_a_record_nothing_is_read_nothing_is_written_and_nothing_is_said() {
        let world = World::installed(false);
        assert!(desktop_preview(&world.io).is_none());
        assert!(world.run().is_empty());
        assert_eq!(*world.settings.sets.lock().unwrap(), 0);
        for index in 0..4 {
            assert!(
                world.path(index).exists(),
                "files without a record are not ours"
            );
        }
    }

    #[test]
    fn without_a_current_proof_everything_is_kept_and_said_so() {
        let world = World::installed(true);
        let none: Arc<dyn Support> = Arc::new(Fixed(Mutex::new(None)));
        let lines = remove_desktop(&world.io, &world.env, &none, None).join(" ");
        assert!(lines.contains("were kept"), "{lines}");
        assert_eq!(world.settings.list.lock().unwrap().len(), 3);
        assert_eq!(*world.settings.sets.lock().unwrap(), 0);
        for index in 0..4 {
            assert!(world.path(index).exists());
        }
        assert!(desktop_record(&world.io).exists());
    }

    #[test]
    fn a_file_the_person_changed_is_kept_and_a_failed_switch_does_not_stop_the_removal() {
        let world = World::installed(true);
        std::fs::write(world.path(1), "// my own edit\n").unwrap();
        *world.settings.fail_set.lock().unwrap() = true;
        let lines = world.run().join(" ");
        assert!(lines.contains("couldn't be turned off"), "{lines}");
        assert!(lines.contains("3 desktop file(s) were removed"), "{lines}");
        assert!(lines.contains("1 desktop file(s) were changed"), "{lines}");
        assert_eq!(
            std::fs::read_to_string(world.path(1)).unwrap(),
            "// my own edit\n"
        );
        assert!(!world.path(0).exists() && !world.path(2).exists() && !world.path(3).exists());
        // The entry stays on (the write failed); the other two are untouched.
        assert_eq!(world.settings.list.lock().unwrap().len(), 3);
        assert!(!desktop_record(&world.io).exists());
    }
}
