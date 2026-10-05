//! Read-only selected-user observation and inventory; never cleanup authority.
use super::{
    audio_package::*, launch_agent::render_plist, native_io::*, payload::*,
    transport::SelectedAgent,
};
use crate::agent_contract::{AgentReply, DecodedReply, StatusAdmission};
use crosspane_installer_core::{InstallReceipt, OperationId, ResourceOwnership};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceState {
    Absent,
    Owned,
    Adopted,
    Foreign,
    Changed,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserResource {
    pub id: String,
    pub path: PathBuf,
    pub state: ResourceState,
    pub identity: Option<FileIdentity>,
    pub sha256: Option<[u8; 32]>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    pub input: bool,
    pub projections: usize,
    pub audio: bool,
    pub instance: u64,
    pub reply_id: u64,
    pub received_at_ms: u64,
    pub epochs: [u64; 4],
    pub recovery_pending: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    Absent,
    LoadedStopped,
    Running(u32),
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageIdentity {
    pub version: String,
    pub manifest_sha256: [u8; 32],
    pub removal_sha256: [u8; 32],
    pub source: FileIdentity,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalInventory {
    pub resources: Vec<UserResource>,
    pub service: ServiceState,
    pub disabled: Option<bool>,
    pub activity: Option<Activity>,
    pub package: PackageIdentity,
    pub audio: AudioPreview,
    pub manifest_sha256: [u8; 32],
}
pub struct RemovalObservation {
    owner: Arc<()>,
    facts: RemovalInventory,
    original: Option<Arc<TrackedAgent>>,
    session: SupportObservation,
    expires: Instant,
    clock_expiry: u64,
}
struct Sources {
    owner: Arc<()>,
    io: Arc<MacNativeIo>,
    approved: ApprovedInventory,
    payload: MacPayload,
    main: SignatureProof,
    session: SupportObservation,
    signing: SigningRequirement,
    audio: Mutex<MacAudioPackage>,
    audio_source: PathBuf,
}
pub struct MacRemovalObserver {
    sources: Arc<Sources>,
}
macro_rules! opaque { ($($t:ty),+) => { $(impl std::fmt::Debug for $t {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(stringify!($t)) }
})+ }; }
opaque!(RemovalObservation, MacRemovalObserver);
fn sha(bytes: &[u8]) -> [u8; 32] {
    let mut result = [0; 32];
    result.copy_from_slice(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes).as_ref());
    result
}
static INVENTORIES: AtomicUsize = AtomicUsize::new(0);
struct Slot;
impl Drop for Slot {
    fn drop(&mut self) {
        INVENTORIES.fetch_sub(1, Ordering::Release);
    }
}
fn bounded<T: Send + 'static>(
    deadline: &Deadline,
    work: impl FnOnce() -> NativeResult<T> + Send + 'static,
) -> NativeResult<T> {
    deadline.check()?;
    INVENTORIES
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 4).then_some(n + 1)
        })
        .map_err(|_| NativeError::Busy)?;
    let slot = Slot;
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("removal-inventory".into())
        .spawn(move || {
            let result = work();
            drop(slot); // Completed work releases admission before its result becomes visible.
            let _ = send.send(result);
        })
        .map_err(|_| NativeError::Unavailable)?;
    loop {
        deadline.check()?;
        match receive.recv_timeout(Duration::from_millis(2)) {
            Ok(result) => {
                deadline.check()?;
                return result;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(NativeError::Unavailable);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
impl MacRemovalObserver {
    /// Caller-approved inventory/signing table and distribution subtree; construction never mutates.
    pub fn admit(
        io: Arc<MacNativeIo>,
        approved: ApprovedInventory,
        audio_source: PathBuf,
        audio_clock: Arc<dyn AudioClock>,
        deadline: &Deadline,
    ) -> NativeResult<Self> {
        if audio_source
            != io
                .target()
                .paths()
                .payload_root
                .join("Crosspane.app/Contents/Resources/audio")
        {
            return Err(NativeError::Foreign);
        }
        let limit = deadline.clone();
        let sources = bounded(deadline, move || {
            let mut approved = approved;
            approved.files.sort_by(|a, b| a.path.cmp(&b.path));
            let payload = MacPayload::admit(io.clone(), approved.clone(), &limit)?;
            let rule = approved
                .files
                .iter()
                .find(|f| f.path == "Crosspane.app/Contents/MacOS/Crosspane")
                .and_then(|f| f.signing.as_ref())
                .ok_or(NativeError::Invalid)?;
            let signing = SigningRequirement {
                role: ArtifactRole::Agent,
                identifier: rule.identifier.clone(),
                designated_requirement: rule.designated_requirement.clone(),
                entitlements: rule.entitlements.clone(),
            };
            let main = io.admit_main_signature(
                &io.target()
                    .paths()
                    .payload_root
                    .join("Crosspane.app/Contents/MacOS/Crosspane"),
                &signing,
                &limit,
            )?;
            let support = io.admit_support(&main, &limit)?;
            let session = io.support_observation(&limit)?;
            support.check(&io, &limit)?;
            let audio =
                MacAudioPackage::admit(io.clone(), audio_source.clone(), audio_clock, &limit)?;
            Ok(Arc::new(Sources {
                owner: Arc::new(()),
                io,
                approved,
                payload,
                main,
                session,
                signing,
                audio: Mutex::new(audio),
                audio_source,
            }))
        })?;
        Ok(Self { sources })
    }
    /// Entire observation remains in a bounded worker; only the admitted caller supplies Status.
    pub fn observe(
        &self,
        current: Option<(SelectedAgent, AgentReply)>,
        revision: u64,
        operation: OperationId,
        deadline: &Deadline,
    ) -> NativeResult<RemovalObservation> {
        let (sources, limit) = (self.sources.clone(), deadline.clone());
        bounded(deadline, move || {
            sources.snapshot(current, revision, operation, &limit)
        })
    }
}

impl RemovalObservation {
    pub fn inventory(&self) -> &RemovalInventory {
        &self.facts
    }
    pub fn tracked_original(&self) -> Option<Arc<TrackedAgent>> {
        self.original.clone()
    }
    pub fn session(&self) -> &SupportObservation {
        &self.session
    }
    /// Read-only freshness and selected-source check, never cleanup authority.
    pub fn check(&self, observer: &MacRemovalObserver, deadline: &Deadline) -> NativeResult<()> {
        if !Arc::ptr_eq(&self.owner, &observer.sources.owner) {
            return Err(NativeError::Foreign);
        }
        self.check_time(&observer.sources.io, deadline)
    }
}
impl RemovalObservation {
    fn check_time(&self, io: &MacNativeIo, deadline: &Deadline) -> NativeResult<()> {
        deadline.check()?;
        if Instant::now() >= self.expires || io.clock().now_ms() > self.clock_expiry {
            return Err(NativeError::Refused);
        }
        Ok(())
    }
}
impl Sources {
    fn snapshot(
        &self,
        current: Option<(SelectedAgent, AgentReply)>,
        revision: u64,
        operation: OperationId,
        deadline: &Deadline,
    ) -> NativeResult<RemovalObservation> {
        let now = self.io.clock().now_ms();
        let mut activity = None;
        let mut original = None;
        let mut left = SUPPORT_LIFETIME_MS;
        let selected = if let Some((selected, reply)) = current {
            if selected.io.target().paths() != self.io.target().paths()
                || reply.source != self.io.target().source()
                || reply.id == 0
                || reply.observed_at_ms > now
                || now - reply.observed_at_ms >= SUPPORT_LIFETIME_MS
            {
                return Err(NativeError::Foreign);
            }
            let DecodedReply::Status(StatusAdmission::Supported(health)) =
                reply.result.map_err(|_| NativeError::Unavailable)?
            else {
                return Err(NativeError::Unavailable);
            };
            selected
                .instance
                .admit_status(&health.installer().instance)?;
            selected
                .instance
                .revalidate(&selected.io, &selected.support, deadline)?;
            left -= now - reply.observed_at_ms;
            activity = Some(Activity {
                input: health.terminal().controlling.is_some()
                    || health.terminal().controlled_by.is_some(),
                projections: health.terminal().projections.len(),
                audio: !health.installer().audio.active_peers.is_empty(),
                instance: selected.instance.bootstrap().instance_id,
                reply_id: reply.id,
                received_at_ms: reply.observed_at_ms,
                epochs: [
                    health.installer().epochs.gate,
                    health.installer().epochs.grants,
                    health.installer().epochs.layout,
                    health.installer().epochs.backends,
                ],
                recovery_pending: health.installer().recovery_pending,
            });
            Some(selected)
        } else {
            None
        };
        let expires = Instant::now() + Duration::from_millis(left);
        let clock_expiry = now.checked_add(left).ok_or(NativeError::IdExhausted)?;
        self.main.revalidate(&self.io)?;
        let support = self.io.admit_support(&self.main, deadline)?;
        let session = self.io.support_observation(deadline)?;
        if session != self.session {
            return Err(NativeError::Foreign);
        }
        let mut resources = self.resources(deadline)?;
        let service = job(
            &self.io,
            &self.io.execute(
                &CommandSpec::new(
                    self.io.target(),
                    NativeOperation::Launchctl(LaunchctlAction::Print),
                )?,
                None,
                deadline,
            )?,
        );
        let disabled = disabled(&self.io.execute(
            &CommandSpec::new(
                self.io.target(),
                NativeOperation::Launchctl(LaunchctlAction::PrintDisabled),
            )?,
            None,
            deadline,
        )?);
        let installed = self
            .io
            .metadata(&self.io.target().agent_path())?
            .map(|_| {
                self.io.admit_main_signature(
                    &self.io.target().agent_path(),
                    &self.signing,
                    deadline,
                )
            })
            .transpose()?;
        let plist_owned = resources
            .iter()
            .any(|r| r.id == "mac.launch-agent" && r.state == ResourceState::Owned);
        if let (ServiceState::Running(pid), Some(selected)) = (&service, selected) {
            if disabled.is_none() {
                return Err(NativeError::Unavailable);
            }
            if selected.instance.process().pid != *pid {
                return Err(NativeError::Foreign);
            }
            if !plist_owned {
                return Err(NativeError::Foreign);
            }
            original = Some(self.io.track_original(
                &support,
                installed.as_ref().ok_or(NativeError::Unavailable)?,
                selected.instance,
                deadline,
            )?);
        } else if service != ServiceState::Absent && activity.is_some() {
            return Err(NativeError::Unavailable);
        }
        // Installer/config/journals/receipts and previous payloads are retained, never recursively removed.
        for name in [
            "config.toml",
            "trust.json",
            "revocations.json",
            "input.journal",
            "projection-input.journal",
            "last_exit.json",
            "agent.lock",
            "identity.lock",
            "device-key.pk8",
        ] {
            let path = self.io.target().state_dir().join(name);
            let identity = self.io.metadata(&path)?;
            resources.push(UserResource {
                id: format!("keep.{name}"),
                path,
                state: if identity.is_none() {
                    ResourceState::Absent
                } else {
                    ResourceState::Unknown
                },
                identity,
                sha256: None,
            });
        }
        let mut audio = self.audio.lock().map_err(|_| NativeError::Unavailable)?;
        let audio_plan = audio.plan(
            &support,
            AudioPackageKind::Remove,
            revision,
            operation.0,
            deadline,
        )?;
        let manifest = self.io.read(
            &self.audio_source.join("packages.json"),
            1024,
            false,
            deadline,
        )?;
        let package_path = self
            .audio_source
            .join(format!("CrosspaneAudio-remove-{}.pkg", audio.version()));
        let source = self
            .io
            .metadata(&package_path)?
            .ok_or(NativeError::Unavailable)?;
        let removal_sha256 = sha(&self
            .io
            .read(&package_path, MAX_FILE_BYTES, false, deadline)?);
        if self.io.metadata(&package_path)? != Some(source.clone()) {
            return Err(NativeError::Foreign);
        }
        let facts = RemovalInventory {
            resources,
            service,
            disabled,
            activity,
            audio: audio_plan.preview().clone(),
            package: PackageIdentity {
                version: audio.version().into(),
                manifest_sha256: sha(&manifest),
                removal_sha256,
                source,
            },
            manifest_sha256: self.payload.manifest_sha256(),
        };
        let snapshot = RemovalObservation {
            owner: self.owner.clone(),
            facts,
            original,
            session,
            expires,
            clock_expiry,
        };
        support.check(&self.io, deadline)?;
        snapshot.check_time(&self.io, deadline)?;
        Ok(snapshot)
    }
    fn resources(&self, deadline: &Deadline) -> NativeResult<Vec<UserResource>> {
        let recovery = self.payload.recovery(deadline)?;
        let receipt = recovery
            .record
            .as_ref()
            .filter(|r| r.phase == PayloadPhase::Verified)
            .map(|r| &r.receipt);
        let mut rows = Vec::new();
        for file in self
            .approved
            .files
            .iter()
            .filter(|f| f.path != "crosspane-installer")
        {
            let (path, id, root) = if file.path == "crosspanectl" {
                (
                    self.io
                        .target()
                        .paths()
                        .home
                        .join(".local/bin/crosspanectl"),
                    "mac.ctl",
                    self.io
                        .target()
                        .paths()
                        .home
                        .join(".local/bin/crosspanectl"),
                )
            } else {
                (
                    self.io
                        .target()
                        .paths()
                        .home
                        .join("Applications")
                        .join(&file.path),
                    "mac.app",
                    self.io.target().app_path(),
                )
            };
            rows.push(resource(
                &self.io,
                file.path.clone(),
                path,
                Some((file.size, file.mode, file.sha256)),
                ownership(
                    receipt,
                    id,
                    &root,
                    self.payload.manifest_sha256(),
                    &self.approved,
                    self.io.target(),
                ),
                deadline,
            ));
        }
        let plist = self
            .io
            .target()
            .paths()
            .home
            .join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist");
        let xml = render_plist(self.io.target())?;
        #[derive(Deserialize)]
        struct Record {
            phase: super::launch_agent::LaunchPhase,
            receipt: InstallReceipt,
        }
        let record = self
            .io
            .read(
                &self.io.target().installer_dir().join("launch-agent.json"),
                512 * 1024,
                true,
                deadline,
            )
            .ok()
            .and_then(|b| serde_json::from_slice::<Record>(&b).ok());
        rows.push(resource(
            &self.io,
            "mac.launch-agent".into(),
            plist.clone(),
            Some((
                xml.len() as u64,
                self.io.metadata(&plist)?.map_or(0o644, |identity| {
                    if identity.mode & 0o7777 == 0o600 {
                        0o600
                    } else {
                        0o644
                    }
                }),
                sha(&xml),
            )),
            record
                .as_ref()
                .filter(|r| matches!(r.phase, super::launch_agent::LaunchPhase::Observed))
                .and_then(|r| {
                    launch_ownership(
                        &r.receipt,
                        &plist,
                        &self.approved.product_version,
                        self.payload.manifest_sha256(),
                        sha(&xml),
                    )
                }),
            deadline,
        ));
        // Only exact manifest-derived directories may later be pruned, and only when empty.
        let app = self.io.target().app_path();
        let expected: BTreeSet<_> = rows.iter().map(|r| r.path.clone()).collect();
        let mut directories = BTreeSet::from([app.clone()]);
        for file in expected.iter().filter(|p| p.starts_with(&app)) {
            let mut parent = file.parent();
            while let Some(p) = parent.filter(|p| p.starts_with(&app)) {
                directories.insert(p.to_owned());
                parent = p.parent();
            }
        }
        let owned_app = ownership(
            receipt,
            "mac.app",
            &app,
            self.payload.manifest_sha256(),
            &self.approved,
            self.io.target(),
        );
        for path in &directories {
            let identity = self.io.metadata(path)?;
            let state = match &identity {
                None => ResourceState::Absent,
                Some(id)
                    if id.mode & 0o170000 != 0o040000
                        || id.uid != self.io.target().paths().uid
                        || id.mode & 0o022 != 0 =>
                {
                    ResourceState::Foreign
                }
                Some(_) => match owned_app {
                    Some(ResourceOwnership::Created) => ResourceState::Owned,
                    Some(ResourceOwnership::Adopted) => ResourceState::Adopted,
                    _ => ResourceState::Unknown,
                },
            };
            rows.push(UserResource {
                id: "mac.app-directory".into(),
                path: path.clone(),
                state,
                identity,
                sha256: None,
            });
        }
        if self.io.metadata(&app)?.is_some() {
            let mut pending = vec![(app.clone(), 0usize)];
            let mut count = 0usize;
            while let Some((directory, depth)) = pending.pop() {
                if depth > 32 {
                    return Err(NativeError::Oversize);
                }
                let before = self.io.metadata(&directory)?.ok_or(NativeError::Foreign)?;
                let entries = self.io.entries(&directory, 4096, deadline)?;
                if self.io.metadata(&directory)? != Some(before) {
                    return Err(NativeError::Foreign);
                }
                for (name, identity) in entries {
                    count += 1;
                    if count > 4096 {
                        return Err(NativeError::Oversize);
                    }
                    let path = directory.join(name);
                    let is_dir = identity.mode & 0o170000 == 0o040000;
                    if is_dir {
                        pending.push((path.clone(), depth + 1));
                    }
                    if !expected.contains(&path) && !directories.contains(&path) {
                        rows.push(UserResource {
                            id: "keep.extra-app-resource".into(),
                            path,
                            state: ResourceState::Foreign,
                            identity: Some(identity),
                            sha256: None,
                        });
                    }
                }
            }
        }
        for (id, path) in [
            ("keep.installer", self.io.target().installer_dir()),
            (
                "keep.app-previous",
                self.io
                    .target()
                    .paths()
                    .home
                    .join("Applications/.Crosspane.app.crosspane-previous"),
            ),
            (
                "keep.ctl-previous",
                self.io
                    .target()
                    .paths()
                    .home
                    .join(".local/bin/.crosspanectl.crosspane-previous"),
            ),
            (
                "keep.logs",
                self.io.target().paths().home.join("Library/Logs/Crosspane"),
            ),
            (
                "keep.legacy-ctl",
                self.io
                    .target()
                    .paths()
                    .home
                    .join(".cargo/bin/crosspanectl"),
            ),
        ] {
            let identity = self.io.metadata(&path)?;
            rows.push(UserResource {
                id: id.into(),
                path,
                state: if identity.is_none() {
                    ResourceState::Absent
                } else {
                    ResourceState::Unknown
                },
                identity,
                sha256: None,
            });
        }
        for (id, path) in [
            (
                "keep.installer-executable",
                self.io
                    .target()
                    .paths()
                    .payload_root
                    .join("crosspane-installer"),
            ),
            (
                "keep.installed-stage",
                self.io
                    .target()
                    .paths()
                    .home
                    .join("Applications/.Crosspane.app.crosspane-stage"),
            ),
            (
                "keep.ctl-stage",
                self.io
                    .target()
                    .paths()
                    .home
                    .join(".local/bin/.crosspanectl.crosspane-stage"),
            ),
            (
                "keep.launch-agent-stage",
                self.io.target().paths().home.join(
                    "Library/LaunchAgents/.io.frostdev.crosspane.agent.plist.crosspane-stage",
                ),
            ),
        ] {
            rows.push(resource(&self.io, id.into(), path, None, None, deadline));
        }
        // A strictly validated repair hint is this one owned bookkeeping leaf, never authority.
        // Corrupt/foreign records fall through to the unchanged recovery-retention policy.
        if let Ok(Some((path, identity, sha256))) =
            super::repair::removal_hint(&self.io, &self.approved, deadline)
        {
            rows.push(UserResource {
                id: "mac.repair-record".into(),
                path,
                state: ResourceState::Owned,
                identity: Some(identity),
                sha256: Some(sha256),
            });
        }
        // Include exact recovery records and every bounded unknown installer/launch remnant.
        for (id, root) in [
            ("keep.installer-entry", self.io.target().installer_dir()),
            (
                "keep.launch-entry",
                self.io.target().paths().home.join("Library/LaunchAgents"),
            ),
        ] {
            if let Some(before) = self.io.metadata(&root)? {
                let entries = self.io.entries(&root, 4096, deadline)?;
                if self.io.metadata(&root)? != Some(before) {
                    return Err(NativeError::Foreign);
                }
                for (name, identity) in entries {
                    let path = root.join(name);
                    if !rows.iter().any(|r| r.path == path) {
                        rows.push(UserResource {
                            id: id.into(),
                            path,
                            state: ResourceState::Unknown,
                            identity: Some(identity),
                            sha256: None,
                        });
                    }
                }
            }
        }
        rows.push(resource(
            &self.io,
            "keep.agent-log".into(),
            self.io
                .target()
                .paths()
                .home
                .join("Library/Logs/Crosspane/agent.log"),
            None,
            None,
            deadline,
        ));
        for path in recovery.retained_temporaries {
            rows.push(resource(
                &self.io,
                "keep.temporary".into(),
                path,
                None,
                None,
                deadline,
            ));
        }
        Ok(rows)
    }
}
// Consume only the producer's observed unfinished envelope as an ownership hint,
// never as clean-stop, live-health or successful mutation evidence.
fn launch_ownership(
    receipt: &InstallReceipt,
    path: &std::path::Path,
    version: &str,
    manifest: [u8; 32],
    rendered: [u8; 32],
) -> Option<ResourceOwnership> {
    use crosspane_installer_core::{MutationOutcome, ResourceObservation, StepId};
    if receipt.schema_version != 1
        || receipt.operation_id.0 == 0
        || receipt.product_version != version
        || receipt.manifest_sha256 != manifest
        || receipt.payload_sha256 != rendered
        || receipt.resources.len() != 1
        || receipt.unfinished != [StepId(12)]
    {
        return None;
    }
    let row = &receipt.resources[0];
    if row.resource_id != "mac.launch-agent"
        || row.resolved_path != path.to_string_lossy()
        || row.after != ResourceObservation::Matching
        || row.outcome != MutationOutcome::Unknown
    {
        return None;
    }
    match (row.ownership, row.before) {
        (ResourceOwnership::Created, ResourceObservation::Absent)
        | (ResourceOwnership::Adopted, ResourceObservation::Different) => Some(row.ownership),
        _ => None,
    }
}

fn ownership(
    receipt: Option<&InstallReceipt>,
    id: &str,
    path: &std::path::Path,
    manifest: [u8; 32],
    approved: &ApprovedInventory,
    target: &MacTarget,
) -> Option<ResourceOwnership> {
    let expected = [
        ("mac.app", target.app_path()),
        (
            "mac.ctl",
            target.paths().home.join(".local/bin/crosspanectl"),
        ),
    ];
    let r = receipt.filter(|r| {
        r.schema_version == 1
            && r.operation_id.0 != 0
            && r.product_version == approved.product_version
            && r.manifest_sha256 == manifest
            && r.payload_sha256 == approved.payload_digest()
            && r.resources.len() == 2
            && r.unfinished.is_empty()
            && r.resources.iter().zip(&expected).all(|(row, (id, path))| {
                row.resource_id == *id
                    && row.resolved_path == path.to_string_lossy()
                    && row.after == crosspane_installer_core::ResourceObservation::Matching
                    && row.outcome == crosspane_installer_core::MutationOutcome::Verified
                    && matches!(
                        (row.ownership, row.before),
                        (
                            ResourceOwnership::Created,
                            crosspane_installer_core::ResourceObservation::Absent
                        ) | (
                            ResourceOwnership::Adopted,
                            crosspane_installer_core::ResourceObservation::Different
                        )
                    )
            })
    })?;
    r.resources
        .iter()
        .find(|row| row.resource_id == id && row.resolved_path == path.to_string_lossy())
        .map(|row| row.ownership)
}
fn resource(
    io: &MacNativeIo,
    id: String,
    path: PathBuf,
    expected: Option<(u64, u32, [u8; 32])>,
    ownership: Option<ResourceOwnership>,
    deadline: &Deadline,
) -> UserResource {
    let observed = (|| {
        let Some(identity) = io.metadata(&path)? else {
            return Ok((None, None, ResourceState::Absent));
        };
        identity.regular(io.target().paths().uid, false)?;
        let bytes = io.read(&path, MAX_FILE_BYTES, false, deadline)?;
        let hash = sha(&bytes);
        if io.metadata(&path)? != Some(identity.clone()) {
            return Err(NativeError::Foreign);
        }
        let state = if expected.is_none() {
            ResourceState::Unknown
        } else if expected != Some((identity.length, identity.mode & 0o7777, hash)) {
            ResourceState::Changed
        } else {
            match ownership {
                Some(ResourceOwnership::Created) => ResourceState::Owned,
                Some(ResourceOwnership::Adopted) => ResourceState::Adopted,
                _ => ResourceState::Foreign,
            }
        };
        Ok((Some(identity), Some(hash), state))
    })();
    let (identity, sha256, state) = match observed {
        Ok(observed) => observed,
        Err(NativeError::Foreign) => (None, None, ResourceState::Foreign),
        Err(_) => (None, None, ResourceState::Unknown),
    };
    UserResource {
        id,
        path,
        identity,
        sha256,
        state,
    }
}
fn job(io: &MacNativeIo, output: &CommandOutput) -> ServiceState {
    let plist = io
        .target()
        .paths()
        .home
        .join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist");
    let program = io.target().agent_path();
    match super::launchd_observation::job(
        output.code,
        &output.stdout,
        &output.stderr,
        super::launchd_observation::SelectedJob {
            uid: io.target().paths().uid,
            label: AGENT_LABEL,
            plist: &plist,
            program: &program,
        },
    ) {
        super::launchd_observation::JobObservation::Absent => ServiceState::Absent,
        super::launchd_observation::JobObservation::Running(pid) => ServiceState::Running(pid),
        super::launchd_observation::JobObservation::LoadedStopped => ServiceState::LoadedStopped,
        super::launchd_observation::JobObservation::Unknown => ServiceState::Unknown,
    }
}
fn disabled(output: &CommandOutput) -> Option<bool> {
    super::launchd_observation::disabled(output.code, &output.stdout, &output.stderr, AGENT_LABEL)
}

pub const DELETE_IDENTITY_EXPLANATION: &str = "Deleting this machine's identity and pairings requires re-pairing. Remote peers may retain an offline trust entry; this is not remote revocation.";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemovalChoices {
    pub delete_identity: bool,
    pub remove_driver: bool,
}
impl Default for RemovalChoices {
    fn default() -> Self {
        Self {
            delete_identity: false,
            remove_driver: true,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovalEffect {
    DisableOwnedAutostart,
    StopTrackedAgent,
    EraseOnlyAfterCleanExit,
    // Root package alone owns these system paths; no user-file deletion authority.
    RemoveSharedDriverAfterPackageVerification,
    RemovePreviousAfterPackageVerification,
    RemoveOwnedAfterVerification,
    PruneEmptyOwnedAfterVerification,
    KeepRecovery,
    KeepIdentity,
    KeepForeign,
    Absent,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalDelta {
    pub resource: String,
    pub path: Option<PathBuf>,
    pub effect: RemovalEffect,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalPreview {
    pub operation: OperationId,
    pub revision: u64,
    pub deltas: Vec<RemovalDelta>,
    pub activity: Option<Activity>,
    pub audio: AudioPreview,
    pub driver_label: &'static str,
    pub identity_explanation: &'static str,
}
pub struct RemovalPlan {
    owner: Arc<()>,
    binding: Arc<()>,
    generation: u64,
    snapshot: RemovalObservation,
    choices: RemovalChoices,
    preview: RemovalPreview,
}
pub struct RemovalConsent {
    binding: Arc<()>,
}
opaque!(RemovalPlan, RemovalConsent, MacRemoval);
impl RemovalPlan {
    pub fn preview(&self) -> &RemovalPreview {
        &self.preview
    }
    pub fn inventory(&self) -> &RemovalInventory {
        self.snapshot.inventory()
    }
    pub fn tracked_original(&self) -> Option<Arc<TrackedAgent>> {
        self.snapshot.tracked_original()
    }
}
pub struct MacRemoval {
    observer: MacRemovalObserver,
    owner: Arc<()>,
    current: Option<Arc<()>>,
    generation: u64,
    last_revision: u64,
    last_operation: u64,
    activity_floor: (u64, u64),
}
impl MacRemoval {
    /// Consumes the admitted observer; construction performs no I/O.
    pub fn new(observer: MacRemovalObserver) -> Self {
        Self {
            observer,
            owner: Arc::new(()),
            current: None,
            generation: 0,
            last_revision: 0,
            last_operation: 0,
            activity_floor: (0, 0),
        }
    }
    /// Retirement never restores a cached plan, including after a failed observation.
    pub fn retire(&mut self) {
        self.current = None;
    }
    fn check_plan(&self, plan: &RemovalPlan, deadline: &Deadline) -> NativeResult<()> {
        if !Arc::ptr_eq(&self.owner, &plan.owner)
            || self.generation != plan.generation
            || self
                .current
                .as_ref()
                .is_none_or(|b| !Arc::ptr_eq(b, &plan.binding))
        {
            return Err(NativeError::Refused);
        }
        plan.snapshot.check(&self.observer, deadline)
    }
    fn advance_activity(&mut self, activity: &Option<Activity>) -> NativeResult<()> {
        if let Some(a) = activity {
            // A cached Status cannot authorize a fresh generation or final dispatch check.
            if a.reply_id <= self.activity_floor.0 || a.received_at_ms < self.activity_floor.1 {
                return Err(NativeError::Foreign);
            }
            self.activity_floor = (a.reply_id, a.received_at_ms);
        }
        Ok(())
    }
    pub fn plan(
        &mut self,
        revision: u64,
        operation: OperationId,
        choices: RemovalChoices,
        current: Option<(SelectedAgent, AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<RemovalPlan> {
        if revision <= self.last_revision || operation.0 <= self.last_operation {
            return Err(NativeError::Invalid);
        }
        self.retire();
        self.last_revision = revision;
        self.last_operation = operation.0;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(NativeError::IdExhausted)?;
        let snapshot = self
            .observer
            .observe(current, revision, operation, deadline)?;
        self.advance_activity(&snapshot.inventory().activity)?;
        let facts = snapshot.inventory();
        let original = snapshot.tracked_original();
        let find_path = |id: &str| {
            facts
                .resources
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.path.clone())
        };
        let mut deltas = vec![
            RemovalDelta {
                resource: "mac.autostart".into(),
                path: find_path("mac.launch-agent"),
                effect: if facts
                    .resources
                    .iter()
                    .any(|r| r.id == "mac.launch-agent" && r.state == ResourceState::Owned)
                    && facts.service != ServiceState::Unknown
                    && facts.disabled.is_some()
                {
                    RemovalEffect::DisableOwnedAutostart
                } else {
                    RemovalEffect::KeepForeign
                },
            },
            RemovalDelta {
                resource: "mac.agent".into(),
                path: find_path("Crosspane.app/Contents/MacOS/Crosspane"),
                effect: if original.is_some() {
                    RemovalEffect::StopTrackedAgent
                } else {
                    RemovalEffect::KeepRecovery
                },
            },
            // This is a conditional preview, never a CleanAgentExit or permission to erase.
            RemovalDelta {
                resource: "mac.identity-pairings".into(),
                path: find_path("keep.trust.json").and_then(|p| p.parent().map(|p| p.to_owned())),
                effect: if choices.delete_identity && original.is_some() {
                    RemovalEffect::EraseOnlyAfterCleanExit
                } else {
                    RemovalEffect::KeepIdentity
                },
            },
            RemovalDelta {
                resource: "mac.shared-audio".into(),
                path: Some(PathBuf::from(
                    "/Library/Audio/Plug-Ins/HAL/CrosspaneAudio.driver",
                )),
                effect: if choices.remove_driver {
                    RemovalEffect::RemoveSharedDriverAfterPackageVerification
                } else {
                    RemovalEffect::KeepRecovery
                },
            },
            RemovalDelta {
                resource: "mac.shared-audio-previous".into(),
                path: Some(PathBuf::from(
                    "/Library/Application Support/Crosspane/Installer/previous",
                )),
                effect: if choices.remove_driver {
                    RemovalEffect::RemovePreviousAfterPackageVerification
                } else {
                    RemovalEffect::KeepRecovery
                },
            },
        ];
        deltas.extend(facts.resources.iter().map(|r| RemovalDelta {
            resource: r.id.clone(),
            path: Some(r.path.clone()),
            effect: match r.state {
                ResourceState::Absent => RemovalEffect::Absent,
                ResourceState::Adopted | ResourceState::Foreign | ResourceState::Changed => {
                    RemovalEffect::KeepForeign
                }
                ResourceState::Owned if original.is_some() && !r.id.starts_with("keep.") => {
                    if r.id == "mac.app-directory" {
                        RemovalEffect::PruneEmptyOwnedAfterVerification
                    } else {
                        RemovalEffect::RemoveOwnedAfterVerification
                    }
                }
                _ if matches!(
                    r.id.as_str(),
                    "keep.trust.json" | "keep.revocations.json" | "keep.device-key.pk8"
                ) =>
                {
                    RemovalEffect::KeepIdentity
                }
                _ => RemovalEffect::KeepRecovery,
            },
        }));
        snapshot.check(&self.observer, deadline)?;
        let preview = RemovalPreview {
            operation,
            revision,
            deltas,
            activity: facts.activity.clone(),
            audio: facts.audio.clone(),
            driver_label: REMOVE_AUDIO_LABEL,
            identity_explanation: DELETE_IDENTITY_EXPLANATION,
        };
        let binding = Arc::new(());
        self.current = Some(binding.clone());
        Ok(RemovalPlan {
            owner: self.owner.clone(),
            binding,
            generation: self.generation,
            snapshot,
            choices,
            preview,
        })
    }
    /// Acknowledges the exact current choices, global driver effect and interruption.
    /// Explicit deletion is necessary but never supplies clean-exit authority.
    pub fn consent(
        &mut self,
        plan: &RemovalPlan,
        revision: u64,
        operation: OperationId,
        choices: RemovalChoices,
        interruption: bool,
        deadline: &Deadline,
    ) -> NativeResult<RemovalConsent> {
        self.check_plan(plan, deadline)?;
        if revision != plan.preview.revision
            || operation != plan.preview.operation
            || plan.choices != choices
            || ((plan.inventory().service != ServiceState::Absent
                || (plan.choices.remove_driver && plan.preview.audio.interrupts_system_audio))
                && !interruption)
        {
            return Err(NativeError::Refused);
        }
        Ok(RemovalConsent {
            binding: plan.binding.clone(),
        })
    }
    /// Future execution must require this immediately before dispatch. This checks the SAME
    /// observer, package, exact resource identities/bytes, activity and session, not clean exit.
    /// Any failed check permanently retires the generation; uncertainty retains recovery.
    pub fn revalidate(
        &mut self,
        plan: &RemovalPlan,
        consent: &RemovalConsent,
        current: Option<(SelectedAgent, AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let result = (|| {
            self.check_plan(plan, deadline)?;
            if !Arc::ptr_eq(&consent.binding, &plan.binding) {
                return Err(NativeError::Refused);
            }
            let fresh = self.observer.observe(
                current,
                plan.preview.revision,
                plan.preview.operation,
                deadline,
            )?;
            self.advance_activity(&fresh.inventory().activity)?;
            let (a, b) = (plan.inventory(), fresh.inventory());
            let activity_equal = match (&a.activity, &b.activity) {
                (Some(a), Some(b)) => {
                    a.instance == b.instance
                        && a.input == b.input
                        && a.projections == b.projections
                        && a.audio == b.audio
                        && a.epochs == b.epochs
                        && a.recovery_pending == b.recovery_pending
                }
                (None, None) => true,
                _ => false,
            };
            let original_equal = match (plan.tracked_original(), fresh.tracked_original()) {
                (Some(a), Some(b)) => {
                    a.instance_id() == b.instance_id() && a.process() == b.process()
                }
                (None, None) => true,
                _ => false,
            };
            if !activity_equal
                || !original_equal
                || a.resources != b.resources
                || a.service != b.service
                || a.disabled != b.disabled
                || a.package != b.package
                || a.audio != b.audio
                || a.manifest_sha256 != b.manifest_sha256
                || plan.snapshot.session() != fresh.session()
            {
                return Err(NativeError::Foreign);
            }
            plan.snapshot.check(&self.observer, deadline)?;
            fresh.check(&self.observer, deadline)
        })();
        if result.is_err() {
            self.retire();
        }
        result
    }
}

const REMOVAL_RECORD_LIMIT: usize = 512 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RemovalOutcome {
    Pending,
    Completed,
    Absent,
    Kept,
    NotClean,
    Waiting,
    Refused,
    Failed,
    Unknown,
    NotDispatched,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalRecovery {
    pub operation: OperationId,
    pub revision: u64,
    pub rows: Vec<RemovalOutcome>,
    pub original_not_clean: bool,
    pub retained_recovery: bool,
}
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemovalRecord {
    schema_version: u32,
    uid: u32,
    home: PathBuf,
    manifest_sha256: [u8; 32],
    package_sha256: [u8; 32],
    preview_sha256: [u8; 32],
    operation: u64,
    revision: u64,
    rows: Vec<RemovalOutcome>,
    in_flight: Option<usize>,
}
pub struct RemovalJournal {
    state: Arc<Mutex<JournalState>>,
    live: Arc<std::sync::atomic::AtomicBool>,
}
pub struct JournalIntent {
    owner: Arc<Mutex<JournalState>>,
    index: usize,
}
struct JournalState {
    path: PathBuf,
    sources: Arc<Sources>,
    _lock: InstallerLock,
    record: RemovalRecord,
    published: Option<(RemovalRecord, FileIdentity)>,
}
opaque!(RemovalJournal, JournalIntent);
impl MacRemoval {
    /// Journal admission does not dispatch, verify a completed effect or mint clean authority.
    /// The lease consumer first performs full revalidation, then checks facts under this lock.
    pub fn open_removal_journal(
        &self,
        plan: &RemovalPlan,
        consent: &RemovalConsent,
        deadline: &Deadline,
    ) -> NativeResult<RemovalJournal> {
        self.check_plan(plan, deadline)?;
        if !Arc::ptr_eq(&consent.binding, &plan.binding) {
            return Err(NativeError::Refused);
        }
        let deltas: Vec<_> = plan
            .preview
            .deltas
            .iter()
            .map(|delta| (&delta.resource, &delta.path, delta.effect as u8))
            .collect();
        let bytes = serde_json::to_vec(&(
            plan.choices.delete_identity,
            plan.choices.remove_driver,
            deltas,
        ))
        .map_err(|_| NativeError::Invalid)?;
        if bytes.len() > REMOVAL_RECORD_LIMIT
            || plan.preview.deltas.is_empty()
            || plan.preview.deltas.len() > 8192
        {
            return Err(NativeError::Oversize);
        }
        let record = RemovalRecord {
            schema_version: 1,
            uid: self.observer.sources.io.target().paths().uid,
            home: self.observer.sources.io.target().paths().home.clone(),
            manifest_sha256: plan.inventory().manifest_sha256,
            package_sha256: plan.inventory().package.removal_sha256,
            preview_sha256: sha(&bytes),
            operation: plan.preview.operation.0,
            revision: plan.preview.revision,
            rows: vec![RemovalOutcome::Pending; plan.preview.deltas.len()],
            in_flight: None,
        };
        let (sources, limit) = (self.observer.sources.clone(), deadline.clone());
        let path = sources.io.target().installer_dir().join("removal.json");
        bounded(deadline, move || {
            let proof = sources.io.admit_support(&sources.main, &limit)?;
            let lock = sources.io.lock(&proof, &limit)?;
            let published = JournalState::read(&sources, &limit)?;
            if published.as_ref().is_some_and(|(old, _)| {
                old.operation >= record.operation
                    || old.revision >= record.revision
                    || old.in_flight.is_some()
                    || old.rows.iter().any(|row| {
                        !matches!(
                            row,
                            RemovalOutcome::Completed
                                | RemovalOutcome::Absent
                                | RemovalOutcome::Kept
                        )
                    })
            }) {
                return Err(NativeError::Refused);
            }
            Ok(RemovalJournal {
                live: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                state: Arc::new(Mutex::new(JournalState {
                    path,
                    sources,
                    _lock: lock,
                    record,
                    published,
                })),
            })
        })
    }
    /// Read-only hints cannot recover an original watch, ownership or a CleanAgentExit.
    pub fn removal_recovery(&self, deadline: &Deadline) -> NativeResult<Option<RemovalRecovery>> {
        let (sources, limit) = (self.observer.sources.clone(), deadline.clone());
        bounded(deadline, move || {
            let Some((record, _)) = JournalState::read(&sources, &limit)? else {
                return Ok(None);
            };
            Ok(Some(RemovalRecovery {
                operation: OperationId(record.operation),
                revision: record.revision,
                rows: record
                    .rows
                    .into_iter()
                    .map(|row| {
                        if row == RemovalOutcome::Pending {
                            RemovalOutcome::Unknown
                        } else {
                            row
                        }
                    })
                    .collect(),
                original_not_clean: true,
                retained_recovery: true,
            }))
        })
    }
}
impl RemovalJournal {
    fn work<T: Send + 'static>(
        &self,
        deadline: &Deadline,
        work: impl FnOnce(&mut JournalState, &Deadline) -> NativeResult<T> + Send + 'static,
    ) -> NativeResult<T> {
        let (state, live, limit) = (self.state.clone(), self.live.clone(), deadline.clone());
        let result = bounded(deadline, move || {
            let mut state = state.lock().map_err(|_| NativeError::Unavailable)?;
            if !live.load(Ordering::Acquire) {
                return Err(NativeError::Refused);
            }
            let result = work(&mut state, &limit);
            if result.is_err() {
                live.store(false, Ordering::Release);
            }
            result
        });
        if result.is_err() {
            self.live.store(false, Ordering::Release);
        }
        result
    }
    /// The lease publishes this only after its under-lock verification. No partial records.
    pub fn publish_initial(&self, deadline: &Deadline) -> NativeResult<()> {
        self.work(deadline, |state, deadline| {
            if state.published.as_ref().map(|(record, _)| record.operation)
                == Some(state.record.operation)
            {
                return Err(NativeError::Refused);
            }
            state.publish(deadline)
        })
    }
    /// Brackets one exact preview row; it conveys no permission to mutate that resource.
    pub fn record_intent(&self, index: usize, deadline: &Deadline) -> NativeResult<JournalIntent> {
        let owner = self.state.clone();
        let intent = self.work(deadline, move |state, deadline| {
            if state.published.as_ref().map(|(record, _)| record.operation)
                != Some(state.record.operation)
                || state.record.in_flight.is_some()
                || state.record.rows.get(index) != Some(&RemovalOutcome::Pending)
            {
                return Err(NativeError::Refused);
            }
            state.record.in_flight = Some(index);
            state.publish(deadline)?;
            Ok(JournalIntent { owner, index })
        })?;
        // A usable intent and permanent retirement share one atomic acceptance point.
        self.live
            .compare_exchange(true, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| NativeError::Refused)?;
        Ok(intent)
    }
    /// The consumer independently verifies effects before passing any completed outcome.
    pub fn record_outcome(
        &self,
        intent: JournalIntent,
        outcome: RemovalOutcome,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if !Arc::ptr_eq(&self.state, &intent.owner) {
            return Err(NativeError::Foreign);
        }
        let live = self.live.clone();
        self.work(deadline, move |state, deadline| {
            if state.record.in_flight != Some(intent.index) || outcome == RemovalOutcome::Pending {
                return Err(NativeError::Refused);
            }
            state.record.rows[intent.index] = outcome;
            state.record.in_flight = None;
            state.publish(deadline)?;
            if !matches!(
                outcome,
                RemovalOutcome::Completed | RemovalOutcome::Absent | RemovalOutcome::Kept
            ) {
                live.store(false, Ordering::Release);
            }
            Ok(())
        })
    }
}
impl JournalState {
    fn read(
        sources: &Sources,
        deadline: &Deadline,
    ) -> NativeResult<Option<(RemovalRecord, FileIdentity)>> {
        let path = sources.io.target().installer_dir().join("removal.json");
        let Some(identity) = sources.io.metadata(&path)? else {
            return Ok(None);
        };
        if identity.mode & 0o7777 != 0o600
            || identity.uid != sources.io.target().paths().uid
            || identity.links != 1
            || identity.length > REMOVAL_RECORD_LIMIT as u64
        {
            return Err(NativeError::Foreign);
        }
        let bytes = sources
            .io
            .read(&path, REMOVAL_RECORD_LIMIT, true, deadline)?;
        let record: RemovalRecord =
            serde_json::from_slice(&bytes).map_err(|_| NativeError::Foreign)?;
        let package = format!(
            "Crosspane.app/Contents/Resources/audio/CrosspaneAudio-remove-{}.pkg",
            sources
                .audio
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .version()
        );
        let package_sha = sources
            .approved
            .files
            .iter()
            .find(|file| file.path == package)
            .map(|file| file.sha256);
        if sources.io.metadata(&path)? != Some(identity.clone())
            || sources
                .io
                .read(&path, REMOVAL_RECORD_LIMIT, true, deadline)?
                != bytes
            || record.schema_version != 1
            || record.operation == 0
            || record.revision == 0
            || record.uid != sources.io.target().paths().uid
            || record.home != sources.io.target().paths().home
            || record.manifest_sha256 != sources.payload.manifest_sha256()
            || package_sha != Some(record.package_sha256)
            || record.rows.is_empty()
            || record.rows.len() > 8192
            || record.in_flight.is_some_and(|index| {
                index >= record.rows.len() || record.rows[index] != RemovalOutcome::Pending
            })
        {
            return Err(NativeError::Foreign);
        }
        if sources.io.metadata(&path)? != Some(identity.clone()) {
            return Err(NativeError::Foreign);
        }
        deadline.check()?;
        Ok(Some((record, identity)))
    }
    fn publish(&mut self, deadline: &Deadline) -> NativeResult<()> {
        deadline.check()?;
        if self.sources.io.support_observation(deadline)? != self.sources.session {
            return Err(NativeError::Foreign);
        }
        let proof = self
            .sources
            .io
            .admit_support(&self.sources.main, deadline)?;
        let actual = Self::read(&self.sources, deadline)?;
        if actual != self.published {
            return Err(NativeError::Foreign);
        }
        let bytes = serde_json::to_vec(&self.record).map_err(|_| NativeError::Invalid)?;
        if bytes.len() > REMOVAL_RECORD_LIMIT {
            return Err(NativeError::Oversize);
        }
        let identity = self.sources.io.atomic_write(
            &proof,
            &self.path,
            &bytes,
            self.published.as_ref().map(|(_, id)| id),
            deadline,
        )?;
        self.published = Some((self.record.clone(), identity));
        Ok(())
    }
}
static REMOVAL_LEASES: AtomicUsize = AtomicUsize::new(0);
struct LeaseSlot(bool);
impl LeaseSlot {
    fn acquire() -> NativeResult<Self> {
        REMOVAL_LEASES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| NativeError::Busy)?;
        Ok(Self(true))
    }
}
impl Drop for LeaseSlot {
    fn drop(&mut self) {
        if self.0 {
            REMOVAL_LEASES.fetch_sub(1, Ordering::Release);
        }
    }
}
/// An active lease owns the original snapshots and selected flock, never new path ownership.
pub struct MacRemovalLease {
    state: Arc<Mutex<LeaseState>>,
    limit: Deadline,
    pending: bool,
    retired: bool,
    slot: LeaseSlot,
}
pub struct RemovalLeaseIntent {
    owner: Arc<Mutex<LeaseState>>,
    journal: JournalIntent,
    delta: RemovalDelta,
    original: Option<UserResource>,
}
/// Successful effects still require native receipts; this enum cannot mint clean authority.
pub enum RemovalEvidence {
    None,
    Erase(crate::agent_contract::EraseIdentityV1),
    Audio(AudioPackageAttempt),
}
struct ExitEvidence {
    receipt: crate::agent_contract::LastExitV1,
    identity: FileIdentity,
    bytes: Vec<u8>,
    bootstrap: Option<(FileIdentity, Vec<u8>)>,
}
struct LeaseState {
    sources: Arc<Sources>,
    journal: RemovalJournal,
    original: Option<Arc<TrackedAgent>>,
    inventory: RemovalInventory,
    preview: RemovalPreview,
    choices: RemovalChoices,
    rows: Vec<RemovalOutcome>,
    pending: Option<usize>,
    activity_floor: (u64, u64),
    exit: Option<ExitEvidence>,
    erased: bool,
    audio: Option<AudioPackageAttempt>,
    staging: Option<PackageStaging>,
    retained: Vec<(PathBuf, Option<FileIdentity>)>,
}
opaque!(MacRemovalLease, RemovalLeaseIntent, RemovalEvidence);
impl RemovalLeaseIntent {
    pub fn delta(&self) -> &RemovalDelta {
        &self.delta
    }
    /// The dispatcher must give this exact snapshot/hash to verified native leaf removal.
    pub fn original_resource(&self) -> Option<&UserResource> {
        self.original.as_ref()
    }
}
impl Drop for MacRemovalLease {
    fn drop(&mut self) {
        if self.pending || self.retired {
            // No timer/reaper completion can undo quarantine. Process exit releases flock.
            // Four acquired slots bound these deliberately retained original-source graphs.
            std::mem::forget(self.state.clone());
            self.slot.0 = false;
        }
    }
}
impl MacRemoval {
    /// Full frozen revalidation once, then source verification under the journal's flock.
    /// Construction dispatches nothing; every future intent checks current facts again.
    pub fn begin(
        &mut self,
        plan: &RemovalPlan,
        consent: &RemovalConsent,
        current: Option<(SelectedAgent, AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<MacRemovalLease> {
        let prepared = (|| {
            self.revalidate(plan, consent, current.clone(), deadline)?;
            let slot = LeaseSlot::acquire()?;
            let journal = self.open_removal_journal(plan, consent, deadline)?;
            Ok(MacRemovalLease {
                state: Arc::new(Mutex::new(LeaseState {
                    sources: self.observer.sources.clone(),
                    journal,
                    original: plan.tracked_original(),
                    inventory: plan.inventory().clone(),
                    preview: plan.preview.clone(),
                    choices: plan.choices,
                    rows: vec![RemovalOutcome::Pending; plan.preview.deltas.len()],
                    pending: None,
                    activity_floor: self.activity_floor,
                    exit: None,
                    erased: false,
                    audio: None,
                    staging: None,
                    retained: Vec::new(),
                })),
                limit: deadline.clone(),
                pending: false,
                retired: false,
                slot,
            })
        })();
        self.retire(); // A failed begin never revives its consent generation.
        let mut lease = prepared?;
        lease.work(deadline, move |state, limit| {
            state.verify(current, true, limit)?;
            state.journal.publish_initial(limit)
        })?;
        lease.pending = false;
        Ok(lease)
    }
}
impl MacRemovalLease {
    fn work<T: Send + 'static>(
        &mut self,
        deadline: &Deadline,
        work: impl FnOnce(&mut LeaseState, &Deadline) -> NativeResult<T> + Send + 'static,
    ) -> NativeResult<T> {
        if self.retired {
            return Err(NativeError::Refused);
        }
        // Caller flags alone settle acceptance. A late worker cannot release quarantine.
        self.pending = true;
        let (state, limit) = (self.state.clone(), self.limit.clone());
        let result = bounded(deadline, move || {
            limit.check()?;
            let mut state = state.lock().map_err(|_| NativeError::Unavailable)?;
            let result = work(&mut state, &limit)?;
            limit.check()?;
            Ok(result)
        })
        .and_then(|value| {
            self.limit.check()?; // Delivery cannot extend the original lease deadline.
            Ok(value)
        });
        if result.is_err() {
            self.retired = true;
        }
        result
    }
    /// Durable, preview-bound preparation only; the future coordinator supplies execution.
    pub fn record_intent(
        &mut self,
        index: usize,
        current: Option<(SelectedAgent, AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<RemovalLeaseIntent> {
        if self.pending || self.retired {
            return Err(NativeError::Refused);
        }
        let owner = self.state.clone();
        self.work(deadline, move |state, limit| {
            let delta = state
                .preview
                .deltas
                .get(index)
                .cloned()
                .ok_or(NativeError::Invalid)?;
            if state.pending.is_some() || state.rows[index] != RemovalOutcome::Pending {
                return Err(NativeError::Refused);
            }
            state.prerequisites(&delta, limit)?;
            state.verify(current, false, limit)?;
            let mut original = state
                .inventory
                .resources
                .iter()
                .find(|r| Some(&r.path) == delta.path.as_ref() && r.id == delta.resource)
                .cloned();
            if delta.effect == RemovalEffect::PruneEmptyOwnedAfterVerification {
                let row = original.as_mut().ok_or(NativeError::Foreign)?;
                // Only our verified child removals can refresh this ORIGINAL directory inode.
                state.resource_identity(row, false, limit)?;
                if !state.sources.io.entries(&row.path, 1, limit)?.is_empty() {
                    return Err(NativeError::Refused);
                }
                let original_identity = row.identity.as_ref().ok_or(NativeError::Foreign)?;
                let refreshed = state
                    .sources
                    .io
                    .metadata(&row.path)?
                    .ok_or(NativeError::Foreign)?;
                if (
                    original_identity.device,
                    original_identity.inode,
                    original_identity.mode,
                    original_identity.uid,
                ) != (
                    refreshed.device,
                    refreshed.inode,
                    refreshed.mode,
                    refreshed.uid,
                ) {
                    return Err(NativeError::Foreign);
                }
                row.identity = Some(refreshed);
            }
            let journal = state.journal.record_intent(index, limit)?;
            state.pending = Some(index);
            Ok(RemovalLeaseIntent {
                owner,
                journal,
                delta,
                original,
            })
        })
    }
    /// Final continuation check after durable intent publication, before any future dispatch.
    pub fn verify_intent(
        &mut self,
        intent: &RemovalLeaseIntent,
        current: Option<(SelectedAgent, AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if !Arc::ptr_eq(&intent.owner, &self.state) {
            return Err(NativeError::Foreign);
        }
        let (index, delta) = (intent.journal.index, intent.delta.clone());
        self.work(deadline, move |state, limit| {
            state.check_intent(index, &delta)?;
            state.prerequisites(&delta, limit)?;
            state.verify(current, false, limit)
        })
    }
    /// Verifies expected post-states, then publishes the native consumer's result.
    pub fn record_outcome(
        &mut self,
        intent: RemovalLeaseIntent,
        outcome: RemovalOutcome,
        evidence: RemovalEvidence,
        current: Option<(SelectedAgent, AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if !Arc::ptr_eq(&intent.owner, &self.state) {
            return Err(NativeError::Foreign);
        }
        let terminal = matches!(
            outcome,
            RemovalOutcome::Completed | RemovalOutcome::Absent | RemovalOutcome::Kept
        );
        let result = self.work(deadline, move |state, limit| {
            let index = intent.journal.index;
            state.check_intent(index, &intent.delta)?;
            if terminal {
                state.accept_effect(&intent.delta, outcome, evidence, limit)?;
                state.rows[index] = outcome;
                state.verify(current, false, limit)?;
            }
            state
                .journal
                .record_outcome(intent.journal, outcome, limit)?;
            state.rows[index] = outcome;
            state.pending = None;
            Ok(())
        });
        if result.is_ok() && terminal {
            self.pending = false;
        }
        if !terminal {
            self.retired = true;
        }
        result
    }
    /// Re-observes only the retained original. Never returns authority once its executable is gone.
    /// The existing one-shot erase must still check original support and receipt at dispatch.
    pub fn clean_exit(
        &mut self,
        intent: &RemovalLeaseIntent,
        deadline: &Deadline,
    ) -> NativeResult<CleanAgentExit> {
        if !Arc::ptr_eq(&intent.owner, &self.state)
            || intent.delta.effect != RemovalEffect::EraseOnlyAfterCleanExit
        {
            return Err(NativeError::Foreign);
        }
        let (index, delta) = (intent.journal.index, intent.delta.clone());
        self.work(deadline, move |state, limit| {
            state.check_intent(index, &delta)?;
            if !state.choices.delete_identity
                || state.erased
                || state
                    .sources
                    .io
                    .metadata(&state.sources.io.target().agent_path())?
                    .is_none()
            {
                return Err(NativeError::Refused);
            }
            let original = state.original.clone().ok_or(NativeError::Refused)?;
            let support = state.support(limit)?;
            let clean = state
                .sources
                .io
                .observe_clean_exit(original, &support, limit)?
                .ok_or(NativeError::Refused)?;
            if state
                .exit
                .as_ref()
                .is_none_or(|cached| cached.receipt != *clean.receipt())
            {
                return Err(NativeError::Foreign);
            }
            Ok(clean)
        })
    }
}
impl LeaseState {
    fn check_intent(&self, index: usize, delta: &RemovalDelta) -> NativeResult<()> {
        if self.pending != Some(index) || self.preview.deltas.get(index) != Some(delta) {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    fn support(&self, deadline: &Deadline) -> NativeResult<SupportProof> {
        if self.sources.io.support_observation(deadline)? != self.sources.session {
            return Err(NativeError::Foreign);
        }
        let proof = self
            .sources
            .io
            .admit_support(&self.sources.main, deadline)?;
        proof.check(&self.sources.io, deadline)?;
        Ok(proof)
    }
    fn completed(&self, effect: RemovalEffect) -> bool {
        self.preview
            .deltas
            .iter()
            .zip(&self.rows)
            .filter(|(d, _)| d.effect == effect)
            .all(|(_, row)| matches!(row, RemovalOutcome::Completed | RemovalOutcome::Absent))
    }
    fn prerequisites(&self, delta: &RemovalDelta, deadline: &Deadline) -> NativeResult<()> {
        use RemovalEffect::*;
        let allowed = match delta.effect {
            StopTrackedAgent => self.completed(DisableOwnedAutostart),
            EraseOnlyAfterCleanExit => {
                self.choices.delete_identity
                    && !self.erased
                    && self.exit.is_some()
                    && self
                        .sources
                        .io
                        .metadata(&self.sources.io.target().agent_path())?
                        .is_some()
            }
            RemoveSharedDriverAfterPackageVerification | RemovePreviousAfterPackageVerification => {
                (self.original.is_none() && self.inventory.service == ServiceState::Absent
                    || self.exit.is_some() && self.completed(StopTrackedAgent))
                    && self.completed(EraseOnlyAfterCleanExit)
            }
            RemoveOwnedAfterVerification | PruneEmptyOwnedAfterVerification => {
                self.exit.is_some()
                    && self.completed(EraseOnlyAfterCleanExit)
                    && self.completed(RemoveSharedDriverAfterPackageVerification)
                    && self.completed(RemovePreviousAfterPackageVerification)
            }
            _ => true,
        };
        deadline.check()?;
        if allowed {
            Ok(())
        } else {
            Err(NativeError::Refused)
        }
    }
    fn accept_effect(
        &mut self,
        delta: &RemovalDelta,
        outcome: RemovalOutcome,
        evidence: RemovalEvidence,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        use RemovalEffect::*;
        if !matches!(
            delta.effect,
            KeepRecovery | KeepIdentity | KeepForeign | Absent
        ) && !matches!(outcome, RemovalOutcome::Completed | RemovalOutcome::Absent)
        {
            return Err(NativeError::Refused);
        }
        match delta.effect {
            KeepRecovery | KeepIdentity | KeepForeign | Absent => {
                if outcome
                    != if delta.effect == Absent {
                        RemovalOutcome::Absent
                    } else {
                        RemovalOutcome::Kept
                    }
                {
                    return Err(NativeError::Refused);
                }
            }
            DisableOwnedAutostart => {
                if self.disabled(deadline)? != Some(true) {
                    return Err(NativeError::Foreign);
                }
            }
            StopTrackedAgent => {
                let original = self.original.clone().ok_or(NativeError::Refused)?;
                let support = self.support(deadline)?;
                let clean = self
                    .sources
                    .io
                    .observe_clean_exit(original, &support, deadline)?
                    .ok_or(NativeError::Refused)?;
                self.exit = Some(self.capture_exit(clean.receipt().clone(), deadline)?);
                // These paths are retained, never promoted to owned deletion rows.
                self.capture_retained(
                    &[
                        "keep.input.journal",
                        "keep.projection-input.journal",
                        "keep.agent.lock",
                        "keep.agent-log",
                        "keep.logs",
                    ],
                    deadline,
                )?;
            }
            EraseOnlyAfterCleanExit => {
                if !self.choices.delete_identity
                    || self.exit.is_none()
                    || self
                        .sources
                        .io
                        .metadata(&self.sources.io.target().agent_path())?
                        .is_none()
                {
                    return Err(NativeError::Refused);
                }
                let RemovalEvidence::Erase(receipt) = evidence else {
                    return Err(NativeError::Refused);
                };
                if !receipt.identity_and_pairings_removed() {
                    return Err(NativeError::Refused);
                }
                self.erased = true;
                self.capture_retained(&["keep.identity.lock"], deadline)?;
            }
            RemoveSharedDriverAfterPackageVerification | RemovePreviousAfterPackageVerification => {
                if !self.choices.remove_driver {
                    return Err(NativeError::Refused);
                }
                if let RemovalEvidence::Audio(attempt) = evidence {
                    self.audio = Some(attempt);
                }
                self.check_audio(deadline)?;
                if self.audio.is_none() {
                    return Err(NativeError::Refused);
                }
            }
            RemoveOwnedAfterVerification | PruneEmptyOwnedAfterVerification => {
                if self.exit.is_none()
                    || self
                        .sources
                        .io
                        .metadata(delta.path.as_ref().ok_or(NativeError::Invalid)?)?
                        .is_some()
                {
                    return Err(NativeError::Foreign);
                }
            }
        }
        Ok(())
    }
    fn capture_retained(&mut self, ids: &[&str], deadline: &Deadline) -> NativeResult<()> {
        for row in self
            .inventory
            .resources
            .iter()
            .filter(|r| ids.contains(&r.id.as_str()))
        {
            let identity = self.sources.io.metadata(&row.path)?;
            if let Some(id) = &identity
                && (id.uid != self.sources.io.target().paths().uid
                    || id.mode & 0o022 != 0
                    || !matches!(id.mode & 0o170000, 0o040000 | 0o100000))
            {
                return Err(NativeError::Foreign);
            }
            self.retained.push((row.path.clone(), identity));
        }
        deadline.check()
    }
    fn disabled(&self, deadline: &Deadline) -> NativeResult<Option<bool>> {
        Ok(disabled(&self.sources.io.execute(
            &CommandSpec::new(
                self.sources.io.target(),
                NativeOperation::Launchctl(LaunchctlAction::PrintDisabled),
            )?,
            None,
            deadline,
        )?))
    }
    fn service(&self, deadline: &Deadline) -> NativeResult<ServiceState> {
        Ok(job(
            &self.sources.io,
            &self.sources.io.execute(
                &CommandSpec::new(
                    self.sources.io.target(),
                    NativeOperation::Launchctl(LaunchctlAction::Print),
                )?,
                None,
                deadline,
            )?,
        ))
    }
    fn capture_exit(
        &self,
        receipt: crate::agent_contract::LastExitV1,
        deadline: &Deadline,
    ) -> NativeResult<ExitEvidence> {
        let path = self.sources.io.target().state_dir().join("last_exit.json");
        let identity = self
            .sources
            .io
            .metadata(&path)?
            .ok_or(NativeError::Foreign)?;
        let bytes = self.sources.io.read(&path, 4096, true, deadline)?;
        if crate::agent_contract::parse_last_exit(&bytes).map_err(|_| NativeError::Invalid)?
            != receipt
            || self.sources.io.metadata(&path)? != Some(identity.clone())
        {
            return Err(NativeError::Foreign);
        }
        let bootstrap = self.bootstrap(deadline)?;
        Ok(ExitEvidence {
            receipt,
            identity,
            bytes,
            bootstrap,
        })
    }
    fn bootstrap(&self, deadline: &Deadline) -> NativeResult<Option<(FileIdentity, Vec<u8>)>> {
        let path = self
            .sources
            .io
            .target()
            .runtime_dir()
            .join("bootstrap.json");
        let Some(identity) = self.sources.io.metadata(&path)? else {
            return Ok(None);
        };
        let bytes = self.sources.io.read(&path, 4096, true, deadline)?;
        if self.sources.io.metadata(&path)? != Some(identity.clone()) {
            return Err(NativeError::Foreign);
        }
        Ok(Some((identity, bytes)))
    }
    fn check_exit(&self, deadline: &Deadline) -> NativeResult<()> {
        let Some(cached) = &self.exit else {
            return Ok(());
        };
        let original = self.original.as_ref().ok_or(NativeError::Foreign)?;
        // After executable removal these are evidence checks only, never a new CleanAgentExit.
        for _ in 0..2 {
            let output = self.sources.io.execute(
                &CommandSpec::new(
                    self.sources.io.target(),
                    NativeOperation::Process {
                        pid: original.process().pid,
                        field: PsField::Uid,
                    },
                )?,
                None,
                deadline,
            )?;
            if output.code != Some(1) || !output.stdout.is_empty() || !output.stderr.is_empty() {
                return Err(NativeError::Foreign);
            }
        }
        let path = self.sources.io.target().state_dir().join("last_exit.json");
        if self.sources.io.metadata(&path)? != Some(cached.identity.clone())
            || self.sources.io.read(&path, 4096, true, deadline)? != cached.bytes
            || self.sources.io.metadata(&path)? != Some(cached.identity.clone())
            || self.bootstrap(deadline)? != cached.bootstrap
        {
            return Err(NativeError::Foreign);
        }
        deadline.check()
    }
    fn check_audio(&mut self, deadline: &Deadline) -> NativeResult<()> {
        if let Some(attempt) = &mut self.audio {
            self.sources
                .audio
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .observe(attempt, deadline)?;
            if attempt.facts().kind != AudioPackageKind::Remove
                || attempt.facts().error.is_some()
                || !matches!(
                    attempt.facts().state,
                    PackageState::Outcome(AudioOutcome::Removed | AudioOutcome::Absent)
                )
            {
                return Err(NativeError::OutcomeUnknown);
            }
            for path in [
                "/Library/Audio/Plug-Ins/HAL/CrosspaneAudio.driver",
                "/Library/Application Support/Crosspane/Installer/previous",
            ] {
                if self
                    .sources
                    .io
                    .audio_metadata(std::path::Path::new(path), deadline)?
                    .is_some()
                {
                    return Err(NativeError::Foreign);
                }
            }
        } else {
            let support = self.support(deadline)?;
            let plan = self
                .sources
                .audio
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .plan(
                    &support,
                    AudioPackageKind::Remove,
                    self.preview.revision,
                    self.preview.operation.0,
                    deadline,
                )?;
            if plan.preview() != &self.inventory.audio {
                return Err(NativeError::Foreign);
            }
        }
        Ok(())
    }
    fn activity(
        &mut self,
        current: Option<(SelectedAgent, AgentReply)>,
        initial: bool,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if self.exit.is_some() {
            return if current.is_none() {
                Ok(())
            } else {
                Err(NativeError::Foreign)
            };
        }
        let Some(expected) = &self.inventory.activity else {
            return if current.is_none() {
                Ok(())
            } else {
                Err(NativeError::Foreign)
            };
        };
        let (selected, reply) = current.ok_or(NativeError::Refused)?;
        let now = self.sources.io.clock().now_ms();
        let fresh_id = if initial {
            reply.id == self.activity_floor.0
        } else {
            reply.id > self.activity_floor.0
        };
        if !fresh_id
            || reply.observed_at_ms < self.activity_floor.1
            || reply.observed_at_ms > now
            || now - reply.observed_at_ms >= SUPPORT_LIFETIME_MS
            || reply.source != self.sources.io.target().source()
            || selected.io.target().paths() != self.sources.io.target().paths()
        {
            return Err(NativeError::Foreign);
        }
        let DecodedReply::Status(StatusAdmission::Supported(health)) =
            reply.result.map_err(|_| NativeError::Unavailable)?
        else {
            return Err(NativeError::Unavailable);
        };
        let original = self.original.as_ref().ok_or(NativeError::Foreign)?;
        if selected.instance.process() != original.process()
            || selected.instance.bootstrap().instance_id != expected.instance
        {
            return Err(NativeError::Foreign);
        }
        selected
            .instance
            .admit_status(&health.installer().instance)?;
        selected
            .instance
            .revalidate(&selected.io, &selected.support, deadline)?;
        if expected.input
            != (health.terminal().controlling.is_some()
                || health.terminal().controlled_by.is_some())
            || expected.projections != health.terminal().projections.len()
            || expected.audio != !health.installer().audio.active_peers.is_empty()
            || expected.recovery_pending != health.installer().recovery_pending
            || expected.epochs
                != [
                    health.installer().epochs.gate,
                    health.installer().epochs.grants,
                    health.installer().epochs.layout,
                    health.installer().epochs.backends,
                ]
        {
            return Err(NativeError::Foreign);
        }
        self.activity_floor = (reply.id, reply.observed_at_ms);
        deadline.check()
    }
    fn resource_identity(
        &self,
        row: &UserResource,
        removed: bool,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let actual = self.sources.io.metadata(&row.path)?;
        if removed {
            return if actual.is_none() {
                Ok(())
            } else {
                Err(NativeError::Foreign)
            };
        }
        let changed_parent = self
            .preview
            .deltas
            .iter()
            .zip(&self.rows)
            .any(|(d, outcome)| {
                *outcome == RemovalOutcome::Completed
                    && d.path
                        .as_ref()
                        .is_some_and(|p| p != &row.path && p.starts_with(&row.path))
            });
        if row.path == self.sources.io.target().installer_dir().join("packages")
            && let Some(attempt) = &self.audio
        {
            return self.verify_package_staging(attempt, deadline);
        }
        let own_installer = row.path == self.sources.io.target().installer_dir();
        let directory = row
            .identity
            .as_ref()
            .is_some_and(|id| id.mode & 0o170000 == 0o040000);
        if directory && (changed_parent || own_installer) {
            if let (Some(old), Some(now)) = (&row.identity, &actual) {
                if (old.device, old.inode, old.mode, old.uid)
                    != (now.device, now.inode, now.mode, now.uid)
                {
                    return Err(NativeError::Foreign);
                }
            } else {
                return Err(NativeError::Foreign);
            }
        } else if actual != row.identity {
            return Err(NativeError::Foreign);
        }
        if let Some(hash) = row.sha256
            && (sha(&self
                .sources
                .io
                .read(&row.path, MAX_FILE_BYTES, false, deadline)?)
                != hash
                || self.sources.io.metadata(&row.path)? != actual)
        {
            return Err(NativeError::Foreign);
        }
        deadline.check()
    }
    fn removed(&self, path: &std::path::Path) -> bool {
        self.preview.deltas.iter().zip(&self.rows).any(|(d, row)| {
            d.path.as_deref() == Some(path)
                && *row == RemovalOutcome::Completed
                && matches!(
                    d.effect,
                    RemovalEffect::RemoveOwnedAfterVerification
                        | RemovalEffect::PruneEmptyOwnedAfterVerification
                )
        })
    }
    fn check_tree(&self, deadline: &Deadline) -> NativeResult<()> {
        let root = self.sources.io.target().app_path();
        if self.sources.io.metadata(&root)?.is_none() {
            return if self.removed(&root)
                || self
                    .inventory
                    .resources
                    .iter()
                    .all(|r| r.path != root || r.identity.is_none())
            {
                Ok(())
            } else {
                Err(NativeError::Foreign)
            };
        }
        let mut pending = vec![(root, 0usize)];
        let mut count = 0usize;
        while let Some((path, depth)) = pending.pop() {
            if depth > 32 {
                return Err(NativeError::Oversize);
            }
            let before = self
                .sources
                .io
                .metadata(&path)?
                .ok_or(NativeError::Foreign)?;
            for (name, identity) in self.sources.io.entries(&path, 4096, deadline)? {
                count += 1;
                if count > 4096 {
                    return Err(NativeError::Oversize);
                }
                let child = path.join(name);
                if self.removed(&child) || !self.inventory.resources.iter().any(|r| r.path == child)
                {
                    return Err(NativeError::Foreign);
                }
                if identity.mode & 0o170000 == 0o040000 {
                    pending.push((child, depth + 1));
                }
            }
            if self.sources.io.metadata(&path)? != Some(before) {
                return Err(NativeError::Foreign);
            }
        }
        deadline.check()
    }
    fn verify(
        &mut self,
        current: Option<(SelectedAgent, AgentReply)>,
        initial: bool,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let support = self.support(deadline)?;
        self.activity(current, initial, deadline)?;
        let expected_service = if self.exit.is_some() {
            ServiceState::Absent
        } else {
            self.inventory.service
        };
        let expected_disabled = if self.completed(RemovalEffect::DisableOwnedAutostart)
            && self
                .preview
                .deltas
                .iter()
                .any(|d| d.effect == RemovalEffect::DisableOwnedAutostart)
        {
            Some(true)
        } else {
            self.inventory.disabled
        };
        if self.service(deadline)? != expected_service
            || self.disabled(deadline)? != expected_disabled
        {
            return Err(NativeError::Foreign);
        }
        self.check_exit(deadline)?;
        self.check_audio(deadline)?;
        if let Some(attempt) = &self.audio {
            self.verify_package_staging(attempt, deadline)?;
        }
        for row in &self.inventory.resources {
            let removed = self.preview.deltas.iter().zip(&self.rows).any(|(d, code)| {
                d.path.as_ref() == Some(&row.path)
                    && d.resource == row.id
                    && *code == RemovalOutcome::Completed
                    && matches!(
                        d.effect,
                        RemovalEffect::RemoveOwnedAfterVerification
                            | RemovalEffect::PruneEmptyOwnedAfterVerification
                    )
            });
            if row.path
                == self
                    .sources
                    .io
                    .target()
                    .installer_dir()
                    .join("removal.json")
            {
                continue; // Managed only by the frozen strict journal below, never path ownership.
            }
            if self.exit.is_some() && row.id == "keep.last_exit.json" {
                continue;
            }
            if let Some((_, expected)) = self.retained.iter().find(|(p, _)| p == &row.path) {
                if self.sources.io.metadata(&row.path)? != *expected {
                    return Err(NativeError::Foreign);
                }
                continue;
            }
            if self.erased
                && matches!(
                    row.id.as_str(),
                    "keep.trust.json" | "keep.revocations.json" | "keep.device-key.pk8"
                )
            {
                if self.sources.io.metadata(&row.path)?.is_some() {
                    return Err(NativeError::Foreign);
                }
            } else {
                self.resource_identity(row, removed, deadline)?;
            }
        }
        self.check_tree(deadline)?;
        let source = self.sources.audio_source.join(format!(
            "CrosspaneAudio-remove-{}.pkg",
            self.inventory.package.version
        ));
        if self.sources.io.metadata(&source)? != Some(self.inventory.package.source.clone())
            || sha(&self
                .sources
                .io
                .read(&source, MAX_FILE_BYTES, false, deadline)?)
                != self.inventory.package.removal_sha256
            || sha(&self.sources.io.read(
                &self.sources.audio_source.join("packages.json"),
                1024,
                false,
                deadline,
            )?) != self.inventory.package.manifest_sha256
        {
            return Err(NativeError::Foreign);
        }
        let actual = JournalState::read(&self.sources, deadline)?;
        let journal = self
            .journal
            .state
            .lock()
            .map_err(|_| NativeError::Unavailable)?;
        if actual != journal.published {
            return Err(NativeError::Foreign);
        }
        drop(journal);
        self.check_exit(deadline)?;
        if self.exit.is_none() && self.inventory.activity.is_some() {
            let now = self.sources.io.clock().now_ms();
            if now < self.activity_floor.1 || now - self.activity_floor.1 >= SUPPORT_LIFETIME_MS {
                return Err(NativeError::Foreign);
            }
        }
        support.check(&self.sources.io, deadline)?;
        deadline.check()
    }
}

// Ordered execution adds no native admission and never reconstructs authority from paths.
/// Supplies current admitted Status replies for the SAME tracked original and caller clock.
/// Implementations belong to the caller; this coordinator owns neither an agent port nor clock.
pub trait RemovalCurrentReader: Send + Sync {
    fn read(
        &self,
        original: &TrackedAgent,
        deadline: &Deadline,
    ) -> NativeResult<(SelectedAgent, AgentReply)>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalApplyRow {
    pub delta: RemovalDelta,
    pub outcome: RemovalOutcome,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalApplyResult {
    pub operation: OperationId,
    pub revision: u64,
    pub rows: Vec<RemovalApplyRow>,
    pub complete: bool,
    pub retained_recovery: bool,
    pub error: Option<NativeError>,
}
impl MacRemoval {
    /// A single explicit attempt. Failure permanently retires the consent/lease, with no resend.
    pub fn apply(
        &mut self,
        plan: &RemovalPlan,
        consent: &RemovalConsent,
        reader: Option<Arc<dyn RemovalCurrentReader>>,
        deadline: &Deadline,
    ) -> NativeResult<RemovalApplyResult> {
        let admission = (|| {
            self.check_plan(plan, deadline)?;
            if !Arc::ptr_eq(&consent.binding, &plan.binding) {
                return Err(NativeError::Refused);
            }
            Ok(())
        })();
        if let Err(error) = admission {
            self.retire();
            return Err(error);
        }
        let initial = read_removal_current(plan.tracked_original(), reader.clone(), deadline);
        let current = match initial {
            Ok(current) => current,
            Err(error) => {
                self.retire();
                return Err(error);
            }
        };
        let preview = plan.preview().clone();
        let lease = self.begin(plan, consent, current, deadline)?;
        Ok(lease.apply_ordered(preview, reader, deadline))
    }
}
fn read_removal_current(
    original: Option<Arc<TrackedAgent>>,
    reader: Option<Arc<dyn RemovalCurrentReader>>,
    deadline: &Deadline,
) -> NativeResult<Option<(SelectedAgent, AgentReply)>> {
    let Some(original) = original else {
        deadline.check()?;
        return Ok(None);
    };
    let reader = reader.ok_or(NativeError::Refused)?;
    let limit = deadline.clone();
    bounded(deadline, move || {
        let current = reader.read(&original, &limit)?;
        limit.check()?;
        Ok(Some(current))
    })
}
fn removal_order(preview: &RemovalPreview) -> Vec<usize> {
    use RemovalEffect::*;
    let mut indices: Vec<_> = (0..preview.deltas.len()).collect();
    indices.sort_by_key(|&index| {
        let delta = &preview.deltas[index];
        let rank = match delta.effect {
            DisableOwnedAutostart => 0,
            StopTrackedAgent => 1,
            EraseOnlyAfterCleanExit => 2,
            RemoveSharedDriverAfterPackageVerification => 3,
            RemovePreviousAfterPackageVerification => 4,
            RemoveOwnedAfterVerification => 5,
            PruneEmptyOwnedAfterVerification => 6,
            _ => 7,
        };
        let depth = if delta.effect == PruneEmptyOwnedAfterVerification {
            std::cmp::Reverse(delta.path.as_ref().map_or(0, |p| p.components().count()))
        } else {
            std::cmp::Reverse(0)
        };
        (rank, depth, index)
    });
    indices
}
impl MacRemovalLease {
    fn apply_ordered(
        mut self,
        preview: RemovalPreview,
        reader: Option<Arc<dyn RemovalCurrentReader>>,
        deadline: &Deadline,
    ) -> RemovalApplyResult {
        let mut result = RemovalApplyResult {
            operation: preview.operation,
            revision: preview.revision,
            rows: preview
                .deltas
                .iter()
                .cloned()
                .map(|delta| RemovalApplyRow {
                    delta,
                    outcome: RemovalOutcome::Pending,
                })
                .collect(),
            complete: false,
            retained_recovery: true,
            error: None,
        };
        for index in removal_order(&preview) {
            let step = self.apply_step(index, reader.clone(), deadline);
            match step {
                Ok(outcome) => result.rows[index].outcome = outcome,
                Err(error) => {
                    result.rows[index].outcome = RemovalOutcome::Unknown;
                    result.error = Some(error);
                    break;
                }
            }
        }
        result.complete = result.error.is_none()
            && result.rows.iter().all(|row| {
                matches!(
                    row.outcome,
                    RemovalOutcome::Completed | RemovalOutcome::Absent | RemovalOutcome::Kept
                )
            });
        // The installer, exact recovery records, logs and configured identity keeps survive.
        result
    }
    fn current_for_dispatch(
        &mut self,
        reader: Option<Arc<dyn RemovalCurrentReader>>,
        deadline: &Deadline,
    ) -> NativeResult<Option<(SelectedAgent, AgentReply)>> {
        deadline.check()?;
        let original = {
            let state = self.state.try_lock().map_err(|_| NativeError::Busy)?;
            if state.exit.is_some() {
                None
            } else {
                state.original.clone()
            }
        };
        let current = read_removal_current(original, reader, deadline);
        if current.is_err() {
            self.retired = true;
        }
        current
    }
    fn apply_step(
        &mut self,
        index: usize,
        reader: Option<Arc<dyn RemovalCurrentReader>>,
        deadline: &Deadline,
    ) -> NativeResult<RemovalOutcome> {
        let current = self.current_for_dispatch(reader.clone(), deadline)?;
        let intent = self.record_intent(index, current, deadline)?;
        let current = self.current_for_dispatch(reader.clone(), deadline)?;
        self.verify_intent(&intent, current, deadline)?;
        let dispatched = self.dispatch_effect(&intent, deadline);
        let (outcome, evidence) = match dispatched {
            Ok(Ok(evidence)) => {
                let outcome = match intent.delta().effect {
                    RemovalEffect::Absent => RemovalOutcome::Absent,
                    RemovalEffect::KeepRecovery
                    | RemovalEffect::KeepIdentity
                    | RemovalEffect::KeepForeign => RemovalOutcome::Kept,
                    _ => RemovalOutcome::Completed,
                };
                (outcome, evidence)
            }
            Ok(Err(error)) => {
                // A failed native attempt is never inferred harmless from exit code or a path.
                self.record_outcome(
                    intent,
                    RemovalOutcome::Unknown,
                    RemovalEvidence::None,
                    None,
                    deadline,
                )?;
                return Err(error);
            }
            Err(error) => return Err(error), // Pending durable intent; lease quarantines forever.
        };
        let current = if intent.delta().effect == RemovalEffect::StopTrackedAgent {
            None // Actual native CleanAgentExit was observed, never a timer or bootout exit code.
        } else {
            self.current_for_dispatch(reader, deadline)?
        };
        self.record_outcome(intent, outcome, evidence, current, deadline)?;
        Ok(outcome)
    }
    fn dispatch_effect(
        &mut self,
        intent: &RemovalLeaseIntent,
        deadline: &Deadline,
    ) -> NativeResult<NativeResult<RemovalEvidence>> {
        if !Arc::ptr_eq(&intent.owner, &self.state) {
            self.retired = true;
            return Err(NativeError::Foreign);
        }
        let clean = if intent.delta.effect == RemovalEffect::EraseOnlyAfterCleanExit {
            Some(self.clean_exit(intent, deadline)?)
        } else {
            None
        };
        let (index, delta, original) = (
            intent.journal.index,
            intent.delta.clone(),
            intent.original.clone(),
        );
        self.work(deadline, move |state, limit| {
            state.check_intent(index, &delta)?;
            state.prerequisites(&delta, limit)?;
            let support = state.support(limit)?;
            let io = state.sources.io.clone();
            let effect = (|| {
                use RemovalEffect::*;
                match delta.effect {
                    KeepRecovery | KeepIdentity | KeepForeign | Absent => Ok(RemovalEvidence::None),
                    DisableOwnedAutostart => {
                        let command = CommandSpec::disable_agent(io.target())?;
                        let output = io.execute(&command, Some(&support), limit)?;
                        if output.code != Some(0) {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        Ok(RemovalEvidence::None)
                    }
                    StopTrackedAgent => {
                        let original = state.original.clone().ok_or(NativeError::Refused)?;
                        let command = CommandSpec::new(
                            io.target(),
                            NativeOperation::Launchctl(LaunchctlAction::Bootout),
                        )?;
                        let output = io.execute(&command, Some(&support), limit)?;
                        if output.code != Some(0) {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        loop {
                            limit.check()?;
                            let support = state.support(limit)?;
                            if io
                                .observe_clean_exit(original.clone(), &support, limit)?
                                .is_some()
                            {
                                return Ok(RemovalEvidence::None);
                            }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                    EraseOnlyAfterCleanExit => {
                        let clean = clean.ok_or(NativeError::Refused)?;
                        let receipt = io.erase_installed_identity(clean, &support, limit)?;
                        if !receipt.identity_and_pairings_removed() {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        Ok(RemovalEvidence::Erase(receipt))
                    }
                    RemoveSharedDriverAfterPackageVerification
                    | RemovePreviousAfterPackageVerification => state.dispatch_audio(index, limit),
                    RemoveOwnedAfterVerification | PruneEmptyOwnedAfterVerification => {
                        let resource = original.as_ref().ok_or(NativeError::Foreign)?;
                        if resource.state != ResourceState::Owned
                            || delta.path.as_ref() != Some(&resource.path)
                            || resource.id != delta.resource
                        {
                            return Err(NativeError::Foreign);
                        }
                        let snapshot = resource.identity.as_ref().ok_or(NativeError::Foreign)?;
                        io.remove_owned_leaf_verified(
                            &support,
                            &resource.path,
                            snapshot,
                            resource.sha256,
                            limit,
                        )?;
                        Ok(RemovalEvidence::None)
                    }
                }
            })();
            // Wrapping an operation error permits a bounded Unknown publication before retirement.
            Ok(effect)
        })
    }
}

struct PackageStaging {
    index: usize,
    original: Option<FileIdentity>,
    entries: Vec<(String, FileIdentity)>,
}
fn digest_hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
impl LeaseState {
    fn prepare_package_staging(&mut self, index: usize, deadline: &Deadline) -> NativeResult<()> {
        if self.audio.is_some() || self.staging.is_some() {
            return Err(NativeError::Refused);
        }
        let delta = self.preview.deltas.get(index).ok_or(NativeError::Invalid)?;
        if !matches!(
            delta.effect,
            RemovalEffect::RemoveSharedDriverAfterPackageVerification
                | RemovalEffect::RemovePreviousAfterPackageVerification
        ) {
            return Err(NativeError::Foreign);
        }
        self.check_intent(index, delta)?;
        let root = self.sources.io.target().installer_dir().join("packages");
        let original = self
            .inventory
            .resources
            .iter()
            .find(|row| row.path == root)
            .and_then(|row| row.identity.clone());
        let actual = self.sources.io.metadata(&root)?;
        if actual != original {
            return Err(NativeError::Foreign);
        }
        let entries = if original.is_some() {
            self.sources.io.entries(&root, 4096, deadline)?
        } else {
            Vec::new()
        };
        if self.sources.io.metadata(&root)? != original {
            return Err(NativeError::Foreign);
        }
        self.staging = Some(PackageStaging {
            index,
            original,
            entries,
        });
        Ok(())
    }
    fn dispatch_audio(
        &mut self,
        index: usize,
        deadline: &Deadline,
    ) -> NativeResult<RemovalEvidence> {
        if self.audio.is_some() {
            self.check_audio(deadline)?;
            return Ok(RemovalEvidence::None); // The second root row never opens Installer again.
        }
        self.prepare_package_staging(index, deadline)?;
        let support = self.support(deadline)?;
        let mut attempt = {
            let mut package = self
                .sources
                .audio
                .lock()
                .map_err(|_| NativeError::Unavailable)?;
            let plan = package.plan(
                &support,
                AudioPackageKind::Remove,
                self.preview.revision,
                self.preview.operation.0,
                deadline,
            )?;
            if plan.preview() != &self.inventory.audio {
                return Err(NativeError::Foreign);
            }
            let consent = plan.consent(
                self.preview.revision,
                self.preview.operation.0,
                self.choices.remove_driver,
                self.choices.remove_driver,
            )?;
            package.open(plan, consent, &support, deadline)?
        };
        let observed = (|| {
            if let Some(error) = attempt.facts().error {
                return Err(error);
            }
            if attempt.staged().is_none() {
                return Err(NativeError::Foreign);
            }
            loop {
                deadline.check()?;
                self.sources
                    .audio
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?
                    .observe(&mut attempt, deadline)?;
                match attempt.facts().state {
                    PackageState::Outcome(AudioOutcome::Removed | AudioOutcome::Absent)
                        if attempt.facts().error.is_none() =>
                    {
                        break;
                    }
                    PackageState::Outcome(_) => return Err(NativeError::OutcomeUnknown),
                    _ if attempt
                        .facts()
                        .error
                        .is_some_and(|error| error != NativeError::OutcomeUnknown) =>
                    {
                        return Err(attempt.facts().error.ok_or(NativeError::OutcomeUnknown)?);
                    }
                    _ => std::thread::sleep(Duration::from_millis(2)),
                }
            }
            self.verify_package_staging(&attempt, deadline)?;
            Ok(())
        })();
        match observed {
            Ok(()) => Ok(RemovalEvidence::Audio(attempt)),
            Err(error) => {
                self.audio = Some(attempt); // Retained with the quarantined source graph.
                Err(error)
            }
        }
    }
    fn verify_package_staging(
        &self,
        attempt: &AudioPackageAttempt,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let witness = self.staging.as_ref().ok_or(NativeError::Foreign)?;
        let delta = self
            .preview
            .deltas
            .get(witness.index)
            .ok_or(NativeError::Foreign)?;
        if !matches!(
            delta.effect,
            RemovalEffect::RemoveSharedDriverAfterPackageVerification
                | RemovalEffect::RemovePreviousAfterPackageVerification
        ) || !(self.pending == Some(witness.index)
            || matches!(
                self.rows[witness.index],
                RemovalOutcome::Completed | RemovalOutcome::Absent
            ))
        {
            return Err(NativeError::Foreign);
        }
        let staged = attempt.staged().ok_or(NativeError::Foreign)?;
        if attempt.facts().kind != AudioPackageKind::Remove
            || staged.sha256 != digest_hex(&self.inventory.package.removal_sha256)
        {
            return Err(NativeError::Foreign);
        }
        let root = self.sources.io.target().installer_dir().join("packages");
        let before = self
            .sources
            .io
            .metadata(&root)?
            .ok_or(NativeError::Foreign)?;
        if (before.device, before.inode) != staged.parent
            || before.uid != self.sources.io.target().paths().uid
            || before.mode != 0o040700
            || witness.original.as_ref().is_some_and(|original| {
                (original.device, original.inode, original.mode, original.uid)
                    != (before.device, before.inode, before.mode, before.uid)
            })
        {
            return Err(NativeError::Foreign);
        }
        let name = format!(
            "CrosspaneAudio-remove-{}.pkg",
            self.inventory.package.version
        );
        let entries = self.sources.io.entries(&root, 4096, deadline)?;
        if entries.len() != witness.entries.iter().filter(|(n, _)| n != &name).count() + 1 {
            return Err(NativeError::Foreign);
        }
        for (child, identity) in &entries {
            if child == &name {
                if identity != &staged.leaf {
                    return Err(NativeError::Foreign);
                }
            } else if !witness
                .entries
                .iter()
                .any(|entry| entry == &(child.clone(), identity.clone()))
            {
                return Err(NativeError::Foreign);
            }
        }
        let path = root.join(&name);
        if self.sources.io.metadata(&path)? != Some(staged.leaf.clone())
            || digest_hex(&sha(&self.sources.io.read(
                &path,
                MAX_FILE_BYTES,
                true,
                deadline,
            )?)) != staged.sha256
            || self.sources.io.metadata(&path)? != Some(staged.leaf.clone())
        {
            return Err(NativeError::Foreign);
        }
        // In-place sibling edits do not change directory metadata; recheck after the leaf read.
        for (child, identity) in &witness.entries {
            deadline.check()?;
            if child != &name
                && self.sources.io.metadata(&root.join(child))? != Some(identity.clone())
            {
                return Err(NativeError::Foreign);
            }
        }
        if self.sources.io.metadata(&root)? != Some(before) {
            return Err(NativeError::Foreign);
        }
        deadline.check()
    }
}
