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
            Some((xml.len() as u64, 0o644, sha(&xml))),
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
    if output.code == Some(113)
        && output.stdout.is_empty()
        && output.stderr
            == format!(
                "Could not find service \"{AGENT_LABEL}\" in domain for user gui: {}\n",
                io.target().paths().uid
            )
            .as_bytes()
    {
        return ServiceState::Absent;
    }
    if output.stdout.len() > 65536
        || output.stderr.len() > 65536
        || output.code != Some(0)
        || !output.stderr.is_empty()
    {
        return ServiceState::Unknown;
    }
    let Ok(text) = std::str::from_utf8(&output.stdout) else {
        return ServiceState::Unknown;
    };
    let expected = format!("gui/{}/{AGENT_LABEL} = {{", io.target().paths().uid);
    if text.lines().next().map(str::trim) != Some(&expected)
        || text.lines().last().map(str::trim) != Some("}")
    {
        return ServiceState::Unknown;
    }
    let mut fields = std::collections::BTreeMap::new();
    let mut depth = 1usize;
    let mut scopes = vec![BTreeSet::new()];
    for line in text.lines().skip(1).map(str::trim) {
        let pair = line.split_once(" = ").or_else(|| line.split_once(" => "));
        if let Some((key, _)) = pair
            && !scopes.last_mut().is_some_and(|scope| scope.insert(key))
        {
            return ServiceState::Unknown;
        }
        if let Some((key, value)) = pair
            && depth == 1
            && matches!(key, "path" | "program" | "pid")
        {
            if fields.insert(key, value).is_some() {
                return ServiceState::Unknown;
            }
            if value != "{" {
                continue;
            }
        }
        if line == "}" {
            let Some(next) = depth.checked_sub(1) else {
                return ServiceState::Unknown;
            };
            depth = next;
            scopes.pop();
        } else if let Some((key, "{")) = pair {
            if depth == 0
                || depth == 32
                || key.is_empty()
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b" ._-".contains(&b))
            {
                return ServiceState::Unknown;
            }
            depth += 1;
            scopes.push(BTreeSet::new());
        } else if depth == 0 || line.contains(['{', '}']) {
            return ServiceState::Unknown;
        }
    }
    if depth != 0 {
        return ServiceState::Unknown;
    }
    let plist = io
        .target()
        .paths()
        .home
        .join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist");
    if fields.get("path").copied() != plist.to_str()
        || fields.get("program").copied() != io.target().agent_path().to_str()
    {
        return ServiceState::Unknown;
    }
    fields
        .get("pid")
        .and_then(|p| p.parse().ok())
        .filter(|p| *p > 0)
        .map(ServiceState::Running)
        .unwrap_or(ServiceState::Unknown)
}
fn disabled(output: &CommandOutput) -> Option<bool> {
    if output.code != Some(0) || !output.stderr.is_empty() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?;
    if text.lines().next()?.trim() != "disabled services = {" || text.lines().last()?.trim() != "}"
    {
        return None;
    }
    let mut value = None;
    let mut keys = BTreeSet::new();
    for line in text
        .lines()
        .skip(1)
        .take(text.lines().count().saturating_sub(2))
    {
        let (key, raw) = line.trim().split_once(" => ")?;
        let key = key.strip_prefix('"')?.strip_suffix('"')?;
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || !keys.insert(key)
        {
            return None;
        }
        let v = match raw {
            "true" => true,
            "false" => false,
            _ => return None,
        };
        if key == AGENT_LABEL {
            value = Some(v);
        }
    }
    Some(value.unwrap_or(false))
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
