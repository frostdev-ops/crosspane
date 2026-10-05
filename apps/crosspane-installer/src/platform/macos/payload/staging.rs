//! Bound consent and publication of the validated app and ctl stages.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadState {
    Absent,
    Matching,
    AdoptionRequired,
    Conflict,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliInventory {
    pub selected: PathBuf,
    pub legacy: Option<PathBuf>,
    pub shadowing: Vec<PathBuf>,
}
/// Private evidence survives an intentional stop; it is never rebound to a replacement instance.
pub struct PayloadPlan {
    pub(super) binding: Arc<()>,
    pub(super) revision: u64,
    pub(super) operation: u64,
    pub(super) app: Tree,
    pub(super) ctl: Tree,
    pub(super) original: Option<Arc<OriginalAgent>>,
    pub(super) baseline_instance: Option<u64>,
    pub(super) dead_runtime: Option<super::super::native_io::DeadRuntime>,
    pub(super) state: PayloadState,
    pub(super) manifest: [u8; 32],
    pub(super) target: TargetPaths,
    pub(super) repair: Option<Arc<inventory::RepairOrigins>>,
    pub(super) origin: Option<Arc<recovery::RecoveryOrigin>>,
}
impl PayloadPlan {
    /// Initial LaunchAgent publication must use the genuine receipt captured by this repair.
    pub(crate) fn repair_launch_origin(&self) -> Option<(&FileIdentity, &[u8])> {
        self.repair
            .as_ref()
            .map(|origin| (&origin.launch.0, origin.launch.1.as_slice()))
    }
    pub(crate) fn owned_files(&self) -> bool {
        self.origin.is_some()
    }
    pub(crate) fn resuming_publication(&self) -> bool {
        self.origin
            .as_ref()
            .is_some_and(|origin| origin.unfinished())
    }
    pub fn state(&self) -> PayloadState {
        self.state
    }
    pub fn view_revision(&self) -> u64 {
        self.revision
    }
    pub fn operation_id(&self) -> u64 {
        self.operation
    }
    pub fn consent(
        &self,
        revision: u64,
        operation: u64,
        adopt_and_interrupt: bool,
    ) -> NativeResult<PayloadConsent> {
        if revision != self.revision
            || operation != self.operation
            || self.state == PayloadState::Conflict
            || (self.state == PayloadState::AdoptionRequired
                && !self.owned_files()
                && !adopt_and_interrupt)
        {
            return Err(NativeError::Refused);
        }
        Ok(PayloadConsent {
            binding: self.binding.clone(),
            revision,
            operation,
            manifest: self.manifest,
        })
    }
}
pub struct PayloadConsent {
    pub(super) binding: Arc<()>,
    pub(super) revision: u64,
    pub(super) operation: u64,
    pub(super) manifest: [u8; 32],
}

impl MacPayload {
    pub fn cli_inventory(
        &self,
        search_path: &[PathBuf],
        deadline: &Deadline,
    ) -> NativeResult<CliInventory> {
        if search_path.len() > 4096 {
            return Err(NativeError::Oversize);
        }
        let selected = self.ctl();
        let legacy_path = self
            .io
            .target()
            .paths()
            .home
            .join(".cargo/bin/crosspanectl");
        let legacy = self.io.metadata(&legacy_path)?.map(|_| legacy_path);
        let mut shadowing = Vec::new();
        for directory in search_path {
            let path = admitted_spelling(directory)?.join(CTL);
            if path == selected {
                break;
            }
            // Foreign/system PATH directories are reported as uninspectable conflicts, never opened.
            if !path.starts_with(&self.io.target().paths().home)
                || self.io.metadata(&path)?.is_some()
            {
                shadowing.push(path);
            }
        }
        deadline.check()?;
        Ok(CliInventory {
            selected,
            legacy,
            shadowing,
        })
    }
    // A token prevents old-instance completion; this read grants no agent admission or authority.
    fn bootstrap_token(&self, deadline: &Deadline) -> NativeResult<Option<u64>> {
        let path = self.io.target().runtime_dir().join("bootstrap.json");
        if self.io.metadata(&path)?.is_none() {
            return Ok(None);
        }
        let bytes = self.io.read(&path, 4096, true, deadline)?;
        Ok(Some(
            parse_bootstrap(&bytes)
                .map_err(|_| NativeError::Invalid)?
                .instance_id,
        ))
    }
    pub fn plan(
        &self,
        revision: u64,
        operation: u64,
        original: Option<Arc<OriginalAgent>>,
        deadline: &Deadline,
    ) -> NativeResult<PayloadPlan> {
        if revision == 0 || operation == 0 {
            return Err(NativeError::Invalid);
        }
        self.support.check(&self.io, deadline)?;
        let baseline_instance = self.bootstrap_token(deadline)?;
        let dead_runtime = self.io.dead_runtime(deadline).ok().flatten();
        self.admit_tree(&self.io.target().paths().payload_root, true, deadline)?;
        let recovery = self.recovery(deadline)?;
        let origin = self.recovery_origin(&recovery, deadline)?;
        if recovery.remnants() || (recovery.unfinished() && origin.is_none()) {
            return Err(NativeError::OutcomeUnknown);
        }
        if original
            .as_ref()
            .is_some_and(|o| o.selected.io.target().paths() != self.io.target().paths())
        {
            return Err(NativeError::Foreign);
        }
        let (app, ctl, matching, conflict) = match self.installed(deadline) {
            Ok((app, ctl, matching)) => (app, ctl, matching, false),
            Err(NativeError::Foreign | NativeError::Unsupported) => (
                tree(&self.io, &self.io.target().app_path(), deadline)?,
                tree(&self.io, &self.ctl(), deadline)?,
                false,
                origin.is_none(),
            ),
            Err(error) => return Err(error),
        };
        let owned = origin.is_some();
        let state = if conflict {
            PayloadState::Conflict
        } else if matching && owned {
            PayloadState::Matching
        } else if app.root.is_none() && ctl.root.is_none() {
            PayloadState::Absent
        } else {
            PayloadState::AdoptionRequired
        };
        Ok(PayloadPlan {
            binding: Arc::new(()),
            revision,
            operation,
            app,
            ctl,
            original,
            baseline_instance,
            dead_runtime,
            state,
            manifest: self.digest,
            target: self.io.target().paths().clone(),
            repair: None,
            origin,
        })
    }
    pub fn install(
        &self,
        plan: PayloadPlan,
        consent: PayloadConsent,
        clean_stop: Option<&CleanStopGate>,
        deadline: &Deadline,
    ) -> NativeResult<Option<PendingPayload>> {
        self.support.check(&self.io, deadline)?;
        if !Arc::ptr_eq(&plan.binding, &consent.binding)
            || plan.revision != consent.revision
            || plan.operation != consent.operation
            || plan.manifest != consent.manifest
            || plan.manifest != self.digest
            || plan.target != *self.io.target().paths()
            || plan.state == PayloadState::Conflict
            || plan.baseline_instance != self.bootstrap_token(deadline)?
        {
            return Err(NativeError::Refused);
        }
        if plan.app != tree(&self.io, &self.io.target().app_path(), deadline)?
            || plan.ctl != tree(&self.io, &self.ctl(), deadline)?
        {
            return Err(NativeError::Foreign);
        }
        if plan.state == PayloadState::Matching
            && plan.repair.is_none()
            && plan.dead_runtime.is_none()
            && !plan.resuming_publication()
        {
            return Ok(None);
        }
        if plan.app.root.is_some()
            && !(plan.state == PayloadState::Matching && plan.repair.is_none())
        {
            let original = plan.original.as_ref().ok_or(NativeError::Refused)?;
            clean_stop
                .ok_or(NativeError::Refused)?
                .check(original, deadline)?;
        }
        self.admit_tree(&self.io.target().paths().payload_root, true, deadline)?;
        // Only private bookkeeping parents/lock precede the flushed intent; no payload target yet.
        self.parents(&self.io.target().installer_dir(), deadline)?;
        let lock = self.io.lock(&self.support, deadline)?;
        if let Some(dead) = &plan.dead_runtime {
            self.io
                .clean_dead_runtime(&self.support, dead, &lock, deadline)?;
        }
        if let Some(origin) = &plan.origin {
            origin.check(self, deadline)?;
        }
        // Newly observed remnants cannot become deletion authority for a reopened receipt.
        let recovery = self.recovery(deadline)?;
        if recovery.remnants() || (recovery.unfinished() && !plan.resuming_publication()) {
            return Err(NativeError::OutcomeUnknown);
        }
        if plan.app != tree(&self.io, &self.io.target().app_path(), deadline)?
            || plan.ctl != tree(&self.io, &self.ctl(), deadline)?
        {
            return Err(NativeError::Foreign);
        }
        if plan.state == PayloadState::Matching && plan.repair.is_none() {
            if plan.resuming_publication() {
                let (app, ctl) = plan.origin.as_ref().ok_or(NativeError::Invalid)?.adopted();
                let record = self.receipt(plan.operation, PayloadPhase::Published, app, ctl);
                self.persist(&record, deadline)?;
                return Ok(Some(PendingPayload {
                    operation: plan.operation,
                    manifest: self.digest,
                    original: plan.original,
                    baseline_instance: plan.baseline_instance,
                    target: plan.target,
                    previous_app: tree(&self.io, &self.app_previous(), deadline)?,
                    previous_ctl: tree(&self.io, &self.ctl_previous(), deadline)?,
                    published_at: self.io.clock().now_ms(),
                    health_call: None,
                    phase: PayloadPhase::Published,
                    repair: None,
                    origins: Some((app, ctl)),
                }));
            }
            return Ok(None);
        }
        if let Some(origins) = &plan.repair {
            origins.check_payload(self, deadline)?;
        }
        let mut record = if plan.repair.is_some() {
            self.repair_receipt(plan.operation, PayloadPhase::Intent)
        } else {
            let (app, ctl) = plan
                .origin
                .as_ref()
                .map(|o| o.adopted())
                .unwrap_or((plan.app.root.is_some(), plan.ctl.root.is_some()));
            self.receipt(plan.operation, PayloadPhase::Intent, app, ctl)
        };
        self.persist(&record, deadline)?;
        let result = (|| {
            self.parents(
                self.app_stage().parent().ok_or(NativeError::Invalid)?,
                deadline,
            )?;
            self.parents(
                self.ctl_stage().parent().ok_or(NativeError::Invalid)?,
                deadline,
            )?;
            self.io
                .create_directory(&self.support, &self.app_stage(), 0o700, deadline)?;
            let dirs = directories(
                self.approved
                    .files
                    .iter()
                    .filter_map(|f| f.path.strip_prefix("Crosspane.app/").map(str::to_owned)),
            );
            for directory in dirs {
                self.parents(&self.app_stage().join(directory), deadline)?;
            }
            for file in self.approved.files.iter().filter(|f| f.path != INSTALLER) {
                let source = self.io.target().paths().payload_root.join(&file.path);
                let bytes = self.io.read(&source, MAX_FILE_BYTES, false, deadline)?;
                if hash(&bytes) != file.sha256 {
                    return Err(NativeError::Foreign);
                }
                let target = if file.path == CTL {
                    self.ctl_stage()
                } else {
                    self.app_stage().join(
                        file.path
                            .strip_prefix("Crosspane.app/")
                            .ok_or(NativeError::Invalid)?,
                    )
                };
                let identity =
                    self.io
                        .atomic_write(&self.support, &target, &bytes, None, deadline)?;
                self.io.finalize_staged_mode(
                    &self.support,
                    &target,
                    &identity,
                    file.mode,
                    deadline,
                )?;
            }
            let (staged_app, staged_ctl) = self.admit_staged(deadline)?;
            record.phase = PayloadPhase::Staged;
            self.persist(&record, deadline)?;
            if tree(&self.io, &self.io.target().app_path(), deadline)? != plan.app
                || tree(&self.io, &self.ctl(), deadline)? != plan.ctl
            {
                return Err(NativeError::Foreign);
            }
            if plan.app.root.is_some() {
                clean_stop.ok_or(NativeError::Refused)?.check(
                    plan.original.as_ref().ok_or(NativeError::Refused)?,
                    deadline,
                )?;
            }
            if let Some(identity) = &plan.app.root {
                self.io.rename_owned(
                    &self.support,
                    &self.io.target().app_path(),
                    &self.app_previous(),
                    identity,
                    deadline,
                )?;
            }
            if let Some(identity) = &plan.ctl.root {
                self.io.rename_owned(
                    &self.support,
                    &self.ctl(),
                    &self.ctl_previous(),
                    identity,
                    deadline,
                )?;
            }
            let previous_app = tree(&self.io, &self.app_previous(), deadline)?;
            let previous_ctl = tree(&self.io, &self.ctl_previous(), deadline)?;
            if !renamed_tree(&plan.app, &previous_app) || !renamed_tree(&plan.ctl, &previous_ctl) {
                return Err(NativeError::Foreign);
            }
            if tree(&self.io, &self.app_stage(), deadline)? != staged_app {
                return Err(NativeError::Foreign);
            }
            self.io.rename_owned(
                &self.support,
                &self.app_stage(),
                &self.io.target().app_path(),
                staged_app.root.as_ref().ok_or(NativeError::Foreign)?,
                deadline,
            )?;
            if tree(&self.io, &self.ctl_stage(), deadline)? != staged_ctl {
                return Err(NativeError::Foreign);
            }
            self.io.rename_owned(
                &self.support,
                &self.ctl_stage(),
                &self.ctl(),
                staged_ctl.root.as_ref().ok_or(NativeError::Foreign)?,
                deadline,
            )?;
            if !self.installed(deadline)?.2 {
                return Err(NativeError::Foreign);
            }
            record.phase = PayloadPhase::Published;
            self.persist(&record, deadline)?;
            if tree(&self.io, &self.app_previous(), deadline)? != previous_app
                || tree(&self.io, &self.ctl_previous(), deadline)? != previous_ctl
            {
                return Err(NativeError::Foreign);
            }
            Ok(PendingPayload {
                operation: plan.operation,
                manifest: self.digest,
                original: plan.original,
                baseline_instance: plan.baseline_instance,
                target: plan.target,
                previous_app,
                previous_ctl,
                published_at: self.io.clock().now_ms(),
                health_call: None,
                phase: PayloadPhase::Published,
                repair: plan.repair,
                origins: plan.origin.as_ref().map(|o| o.adopted()),
            })
        })();
        match result {
            Ok(pending) => Ok(Some(pending)),
            Err(_) => {
                record.phase = PayloadPhase::Unknown;
                // An expired/cancelled receipt write may itself fail; the durable intent remains.
                let _ = self.persist(&record, deadline);
                Err(NativeError::OutcomeUnknown)
            }
        }
    }
    fn admit_staged(&self, deadline: &Deadline) -> NativeResult<(Tree, Tree)> {
        let staged = tree(&self.io, &self.app_stage(), deadline)?;
        let ctl = tree(&self.io, &self.ctl_stage(), deadline)?;
        let expected: BTreeMap<_, _> = self
            .approved
            .files
            .iter()
            .filter_map(|f| {
                f.path
                    .strip_prefix("Crosspane.app/")
                    .map(|p| (p.to_owned(), f.sha256))
            })
            .collect();
        if staged.hashes != expected
            || staged.nodes.keys().cloned().collect::<BTreeSet<_>>()
                != expected
                    .keys()
                    .cloned()
                    .chain(directories(expected.keys().cloned()))
                    .collect()
        {
            return Err(NativeError::Foreign);
        }
        for file in self.approved.files.iter().filter(|f| f.path != INSTALLER) {
            let path = if file.path == CTL {
                self.ctl_stage()
            } else {
                self.app_stage().join(
                    file.path
                        .strip_prefix("Crosspane.app/")
                        .ok_or(NativeError::Invalid)?,
                )
            };
            let id = self.io.metadata(&path)?.ok_or(NativeError::Foreign)?;
            if id.length != file.size
                || id.mode & 0o777 != file.mode
                || hash(&self.io.read(&path, MAX_FILE_BYTES, false, deadline)?) != file.sha256
            {
                return Err(NativeError::Foreign);
            }
            if let Some(rule) = &file.signing {
                self.io
                    .admit_artifact_signature(&path, &rule.native(), &self.main, deadline)?;
            }
        }
        self.bundle(&self.app_stage(), deadline)?;
        if tree(&self.io, &self.app_stage(), deadline)? != staged
            || tree(&self.io, &self.ctl_stage(), deadline)? != ctl
        {
            return Err(NativeError::Foreign);
        }
        Ok((staged, ctl))
    }
}

impl MacPayload {
    pub(crate) fn plan_repair(
        &self,
        revision: u64,
        operation: u64,
        original: Option<Arc<OriginalAgent>>,
        launch: inventory::LaunchOrigin,
        deadline: &Deadline,
    ) -> NativeResult<PayloadPlan> {
        let mut plan = self.plan(revision, operation, original, deadline)?;
        let origins = self.repair_origins(launch, deadline)?;
        let (app, ctl) = self.installed_repair(deadline)?;
        if app.root.is_some() && plan.original.is_none() {
            return Err(NativeError::Refused);
        }
        plan.app = app;
        plan.ctl = ctl;
        plan.state = PayloadState::Matching;
        plan.repair = Some(origins);
        Ok(plan)
    }
    pub(crate) fn check_repair_plan(
        &self,
        plan: &PayloadPlan,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let origins = plan.repair.as_ref().ok_or(NativeError::Invalid)?;
        origins.check_payload(self, deadline)?;
        origins.check_launch(self, deadline)?;
        if tree(&self.io, &self.io.target().app_path(), deadline)? != plan.app
            || tree(&self.io, &self.ctl(), deadline)? != plan.ctl
        {
            return Err(NativeError::Foreign);
        }
        deadline.check()
    }
}
