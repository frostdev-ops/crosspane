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
    pub(super) state: PayloadState,
    pub(super) manifest: [u8; 32],
    pub(super) target: TargetPaths,
}
impl PayloadPlan {
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
            || (self.state == PayloadState::AdoptionRequired && !adopt_and_interrupt)
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
        if search_path.len() > 32 {
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
        self.admit_tree(&self.io.target().paths().payload_root, true, deadline)?;
        let recovery = self.recovery(deadline)?;
        if recovery.unfinished() {
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
                true,
            ),
            Err(error) => return Err(error),
        };
        let owned = recovery.record.as_ref().is_some_and(|r| {
            let receipt = &r.receipt;
            r.phase == PayloadPhase::Verified
                && receipt.operation_id.0 != 0
                && receipt.resources.len() == 2
                && *receipt
                    == self
                        .receipt(
                            receipt.operation_id.0,
                            PayloadPhase::Verified,
                            receipt.resources[0].ownership == ResourceOwnership::Adopted,
                            receipt.resources[1].ownership == ResourceOwnership::Adopted,
                        )
                        .receipt
        });
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
            state,
            manifest: self.digest,
            target: self.io.target().paths().clone(),
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
        if plan.state == PayloadState::Matching {
            return Ok(None);
        }
        if plan.app.root.is_some() {
            let original = plan.original.as_ref().ok_or(NativeError::Refused)?;
            clean_stop
                .ok_or(NativeError::Refused)?
                .check(original, deadline)?;
        }
        self.admit_tree(&self.io.target().paths().payload_root, true, deadline)?;
        // Only private bookkeeping parents/lock precede the flushed intent; no payload target yet.
        self.parents(&self.io.target().installer_dir(), deadline)?;
        let _lock = self.io.lock(&self.support, deadline)?;
        let recovery = self.recovery(deadline)?;
        if recovery.unfinished() {
            return Err(NativeError::OutcomeUnknown);
        }
        let mut record = self.receipt(
            plan.operation,
            PayloadPhase::Intent,
            plan.app.root.is_some(),
            plan.ctl.root.is_some(),
        );
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
