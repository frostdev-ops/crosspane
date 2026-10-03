//! Advisory audio package facts, called on a detached worker. Outcomes never prove working audio.
//! Root scripts alone enforce bundle identity and transact HAL. ACLs are the accepted native residual.
use super::native_io::*;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub const REMOVE_AUDIO_LABEL: &str =
    "Remove the Crosspane audio driver (affects every user on this Mac)";
const ROOT: &str = "/Library/Application Support/Crosspane";
const HAL: &str = "/Library/Audio/Plug-Ins/HAL";
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioPackageKind {
    Install,
    Remove,
}
impl AudioPackageKind {
    fn removal(self) -> bool {
        self == Self::Remove
    }
    fn name(self, version: &str) -> String {
        format!(
            "CrosspaneAudio-{}-{version}.pkg",
            if self.removal() { "remove" } else { "install" }
        )
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Package {
    kind: AudioPackageKind,
    file: String,
    sha256: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    version: String,
    packages: [Package; 2],
}
fn version(value: &str) -> bool {
    let parts: Vec<_> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 4 && p.bytes().all(|b| b.is_ascii_digit()))
}
fn hash(bytes: &[u8]) -> String {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn json<T: serde::de::DeserializeOwned + Serialize>(bytes: &[u8], limit: usize) -> NativeResult<T> {
    if bytes.len() > limit || bytes.last() != Some(&b'\n') {
        return Err(NativeError::Invalid);
    }
    let parsed: T = serde_json::from_slice(bytes).map_err(|_| NativeError::Invalid)?;
    let mut canonical = serde_json::to_vec(&parsed).map_err(|_| NativeError::Invalid)?;
    canonical.push(b'\n');
    if canonical != bytes {
        return Err(NativeError::Invalid);
    }
    Ok(parsed)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Absent,
    Present,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPreview {
    pub driver: Presence,
    /// Contents cannot be inspected: an existing directory may retain a manual previous copy.
    pub previous: Presence,
    pub same_volume: Option<bool>,
    pub safe_metadata: bool,
    pub interrupts_system_audio: bool,
}
type Metadata = Option<(u32, u32, u32, u64)>;
#[derive(Clone, PartialEq, Eq)]
struct Snapshot {
    entries: [Result<Metadata, NativeError>; 6],
}
impl Snapshot {
    fn preview(&self, kind: AudioPackageKind) -> AudioPreview {
        let presence = |index: usize| match self.entries[index] {
            Ok(Some(_)) => Presence::Present,
            Ok(None) => Presence::Absent,
            Err(_) => Presence::Unknown,
        };
        let safe_metadata = self
            .entries
            .iter()
            .enumerate()
            .all(|(i, entry)| match entry {
                Ok(Some((file_kind, uid, mode, _))) => {
                    *file_kind == 0o040000
                        && *uid == 0
                        && mode & 0o7022 == 0
                        && (i != 5 || mode & 0o077 == 0)
                }
                Ok(None) => i > 1,
                Err(_) => false,
            });
        let same_volume = self.entries[0]
            .as_ref()
            .ok()
            .and_then(|m| m.as_ref())
            .map(|hal| {
                self.entries
                    .iter()
                    .flatten()
                    .flatten()
                    .all(|entry| entry.3 == hal.3)
            });
        AudioPreview {
            driver: presence(4),
            previous: presence(5),
            same_volume,
            safe_metadata,
            interrupts_system_audio: !kind.removal()
                || presence(4) != Presence::Absent
                || presence(5) != Presence::Absent,
        }
    }
}
pub struct AudioPackagePlan {
    owner: Arc<()>,
    binding: Arc<()>,
    generation: u64,
    revision: u64,
    operation: u64,
    kind: AudioPackageKind,
    snapshot: Snapshot,
    source: FileIdentity,
    staged: Option<FileIdentity>,
    session: SupportObservation,
    preview: AudioPreview,
}
pub struct AudioPackageConsent {
    binding: Arc<()>,
}
impl AudioPackagePlan {
    pub fn preview(&self) -> &AudioPreview {
        &self.preview
    }
    /// Current-view consent must explain shared-driver replacement/system audio interruption,
    /// retained manual previous copies on install, and their deletion on removal.
    pub fn consent(
        &self,
        revision: u64,
        operation: u64,
        shared_audio: bool,
        remove_previous: bool,
    ) -> NativeResult<AudioPackageConsent> {
        if revision != self.revision
            || operation != self.operation
            || !shared_audio
            || !self.preview.safe_metadata
            || self.preview.same_volume != Some(true)
            || (self.kind.removal()
                && self.preview.previous == Presence::Present
                && !remove_previous)
        {
            return Err(NativeError::Refused);
        }
        Ok(AudioPackageConsent {
            binding: self.binding.clone(),
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioOutcome {
    Installed,
    VerifyFailed,
    MoveFailed,
    Removed,
    Absent,
    RemoveFailed,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallOutcome {
    schema_version: u32,
    result: AudioOutcome,
    driver_version: String,
    at_unix_ms: u64,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RemovalOutcome {
    schema_version: u32,
    result: AudioOutcome,
    at_unix_ms: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageState {
    OpenRequested,
    Unknown,
    Outcome(AudioOutcome),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentEvidence {
    Required,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPackageFacts {
    pub kind: AudioPackageKind,
    pub state: PackageState,
    pub error: Option<NativeError>,
    /// Final selected-peer/tone/UID evidence belongs to WP-4.16/4.22, even after Installed.
    pub agent_evidence: AgentEvidence,
}
type OutcomeIdentity = (u64, u64, i128, u64);
fn identity(file: &FileIdentity) -> OutcomeIdentity {
    (file.device, file.inode, file.modified_ns, file.length)
}
pub struct AudioPackageAttempt {
    owner: Arc<()>,
    prior: Option<OutcomeIdentity>,
    start_ms: u64,
    facts: AudioPackageFacts,
}
impl AudioPackageAttempt {
    pub fn facts(&self) -> &AudioPackageFacts {
        &self.facts
    }
    /// Dismissal never proves that Installer stopped; later observation is still allowed.
    pub fn dismiss(&mut self) {
        self.facts.state = PackageState::Unknown;
    }
}
pub trait AudioClock: Send + Sync {
    fn unix_ms(&self) -> NativeResult<u64>;
}
#[derive(Debug)]
pub struct SystemAudioClock;
impl AudioClock for SystemAudioClock {
    fn unix_ms(&self) -> NativeResult<u64> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok())
            .ok_or(NativeError::Unavailable)
    }
}
pub struct MacAudioPackage {
    io: Arc<MacNativeIo>,
    source: PathBuf,
    manifest: Manifest,
    manifest_identity: FileIdentity,
    owner: Arc<()>,
    generation: u64,
    clock: Arc<dyn AudioClock>,
}
macro_rules! redacted_debug {
    ($($ty:ty),+ $(,)?) => {
        $(impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(stringify!($ty))
            }
        })+
    };
}
redacted_debug!(
    AudioPackagePlan,
    AudioPackageConsent,
    AudioPackageAttempt,
    MacAudioPackage
);
impl MacAudioPackage {
    /// Only an explicit selected-user distribution subtree and production packages.json are read.
    pub fn admit(
        io: Arc<MacNativeIo>,
        source: PathBuf,
        clock: Arc<dyn AudioClock>,
        deadline: &Deadline,
    ) -> NativeResult<Self> {
        if !source.starts_with(&io.target().paths().payload_root) {
            return Err(NativeError::Foreign);
        }
        let path = source.join("packages.json");
        let manifest_identity = io.metadata(&path)?.ok_or(NativeError::Unavailable)?;
        let manifest: Manifest = json(&io.read(&path, 1024, false, deadline)?, 1024)?;
        if manifest.schema_version != 1
            || !version(&manifest.version)
            || manifest
                .packages
                .iter()
                .zip([AudioPackageKind::Install, AudioPackageKind::Remove])
                .any(|(p, kind)| {
                    p.kind != kind
                        || p.file != kind.name(&manifest.version)
                        || p.sha256.len() != 64
                        || !p
                            .sha256
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
            || io.metadata(&path)? != Some(manifest_identity.clone())
        {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            io,
            source,
            manifest,
            manifest_identity,
            owner: Arc::new(()),
            generation: 0,
            clock,
        })
    }
    pub fn version(&self) -> &str {
        &self.manifest.version
    }
    fn package(&self, kind: AudioPackageKind) -> &Package {
        &self.manifest.packages[usize::from(kind.removal())]
    }
    fn destination(&self, kind: AudioPackageKind) -> PathBuf {
        self.io
            .target()
            .installer_dir()
            .join("packages")
            .join(&self.package(kind).file)
    }
    fn snapshot(&self, deadline: &Deadline) -> NativeResult<Snapshot> {
        let mut entries = [Ok(None); 6];
        for (i, path) in [
            HAL.to_owned(),
            "/Library/Application Support".into(),
            ROOT.into(),
            format!("{ROOT}/Installer"),
            format!("{HAL}/CrosspaneAudio.driver"),
            format!("{ROOT}/Installer/previous"),
        ]
        .iter()
        .enumerate()
        {
            deadline.check()?;
            entries[i] = self.io.audio_metadata(Path::new(path), deadline);
        }
        Ok(Snapshot { entries })
    }
    fn source_bytes(
        &self,
        kind: AudioPackageKind,
        deadline: &Deadline,
    ) -> NativeResult<(Vec<u8>, FileIdentity)> {
        if self.io.metadata(&self.source.join("packages.json"))?
            != Some(self.manifest_identity.clone())
        {
            return Err(NativeError::Foreign);
        }
        let package = self.package(kind);
        let path = self.source.join(&package.file);
        let file = self.io.metadata(&path)?.ok_or(NativeError::Unavailable)?;
        let bytes = self.io.read(&path, MAX_FILE_BYTES, false, deadline)?;
        if bytes.is_empty()
            || hash(&bytes) != package.sha256
            || self.io.metadata(&path)? != Some(file.clone())
        {
            return Err(NativeError::Foreign);
        }
        Ok((bytes, file))
    }
    /// Every explicit retry makes a new plan, re-detects fixed resources/outcome, and needs new consent.
    pub fn plan(
        &mut self,
        proof: &SupportProof,
        kind: AudioPackageKind,
        revision: u64,
        operation: u64,
        deadline: &Deadline,
    ) -> NativeResult<AudioPackagePlan> {
        proof.check(&self.io, deadline)?;
        if revision == 0 || operation == 0 {
            return Err(NativeError::Invalid);
        }
        let snapshot = self.snapshot(deadline)?;
        self.io.read_audio_outcome(kind.removal(), deadline)?;
        let (_, source) = self.source_bytes(kind, deadline)?;
        let staged = self.io.metadata(&self.destination(kind))?;
        if staged.is_some()
            && hash(
                &self
                    .io
                    .read(&self.destination(kind), MAX_FILE_BYTES, true, deadline)?,
            ) != self.package(kind).sha256
        {
            return Err(NativeError::Foreign);
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(NativeError::IdExhausted)?;
        let preview = snapshot.preview(kind);
        Ok(AudioPackagePlan {
            owner: self.owner.clone(),
            binding: Arc::new(()),
            generation: self.generation,
            revision,
            operation,
            kind,
            snapshot,
            source,
            staged,
            session: self.io.support_observation(deadline)?,
            preview,
        })
    }
    fn directories(&self, proof: &SupportProof, deadline: &Deadline) -> NativeResult<()> {
        for suffix in [
            "Library",
            "Library/Application Support",
            "Library/Application Support/Crosspane",
            "Library/Application Support/Crosspane/Installer",
            "Library/Application Support/Crosspane/Installer/packages",
        ] {
            let path = self.io.target().paths().home.join(suffix);
            if let Some(s) = self.io.metadata(&path)? {
                if s.mode & 0o170000 != 0o040000
                    || s.uid != self.io.target().paths().uid
                    || s.mode & 0o7022 != 0
                    || (suffix.ends_with("/packages") && s.mode & 0o7777 != 0o700)
                {
                    return Err(NativeError::Foreign);
                }
            } else {
                self.io.create_directory(proof, &path, 0o700, deadline)?;
            }
        }
        Ok(())
    }
    pub fn open(
        &mut self,
        plan: AudioPackagePlan,
        consent: AudioPackageConsent,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<AudioPackageAttempt> {
        if !Arc::ptr_eq(&self.owner, &plan.owner)
            || !Arc::ptr_eq(&plan.binding, &consent.binding)
            || plan.generation != self.generation
        {
            return Err(NativeError::Refused);
        }
        proof.check(&self.io, deadline)?;
        let (bytes, source) = self.source_bytes(plan.kind, deadline)?;
        let path = self.destination(plan.kind);
        if source != plan.source
            || self.io.metadata(&path)? != plan.staged
            || self.snapshot(deadline)? != plan.snapshot
            || self.io.support_observation(deadline)? != plan.session
        {
            return Err(NativeError::Foreign);
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(NativeError::IdExhausted)?;
        let mut attempt = AudioPackageAttempt {
            owner: self.owner.clone(),
            prior: None,
            start_ms: 0,
            facts: AudioPackageFacts {
                kind: plan.kind,
                state: PackageState::Unknown,
                error: None,
                agent_evidence: AgentEvidence::Required,
            },
        };
        let result = (|| {
            self.directories(proof, deadline)?;
            self.io
                .atomic_write(proof, &path, &bytes, plan.staged.as_ref(), deadline)?;
            if hash(&self.io.read(&path, MAX_FILE_BYTES, true, deadline)?)
                != self.package(plan.kind).sha256
            {
                return Err(NativeError::Foreign);
            }
            proof.check(&self.io, deadline)?;
            if self.snapshot(deadline)? != plan.snapshot
                || self.io.support_observation(deadline)? != plan.session
            {
                return Err(NativeError::Foreign);
            }
            attempt.prior = self
                .io
                .read_audio_outcome(plan.kind.removal(), deadline)?
                .as_ref()
                .map(|(_, f)| identity(f));
            attempt.start_ms = self.clock.unix_ms()?;
            // Accepted residual: a same-UID actor can replace this pathname after the hash,
            // despite the private 0700 directory. Same-UID substitution is outside the threat
            // model; the frozen pathname-only handoff cannot pin the bytes Installer reads.
            // Package and outcome facts always retain AgentEvidence::Required.
            let command =
                CommandSpec::new(self.io.target(), NativeOperation::OpenAudioPackage { path })?;
            let output = self.io.execute(&command, Some(proof), deadline)?;
            if output.code != Some(0) {
                return Err(NativeError::Refused);
            }
            Ok(())
        })();
        match result {
            Ok(()) => attempt.facts.state = PackageState::OpenRequested,
            Err(error) => attempt.facts.error = Some(error),
        }
        Ok(attempt)
    }
    /// Advisory progress only. No open, automatic retry, rollback, restore, restart or agent probe.
    pub fn observe(
        &self,
        attempt: &mut AudioPackageAttempt,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if !Arc::ptr_eq(&self.owner, &attempt.owner) {
            return Err(NativeError::Foreign);
        }
        let result = (|| {
            if attempt.start_ms == 0 {
                return Err(NativeError::OutcomeUnknown);
            }
            let (bytes, file) = self
                .io
                .read_audio_outcome(attempt.facts.kind.removal(), deadline)?
                .ok_or(NativeError::OutcomeUnknown)?;
            let (outcome, at) = if attempt.facts.kind.removal() {
                let o: RemovalOutcome = json(&bytes, 256)?;
                if o.schema_version != 1
                    || !matches!(
                        o.result,
                        AudioOutcome::Removed | AudioOutcome::Absent | AudioOutcome::RemoveFailed
                    )
                {
                    return Err(NativeError::Invalid);
                }
                (o.result, o.at_unix_ms)
            } else {
                let o: InstallOutcome = json(&bytes, 256)?;
                if o.schema_version != 1
                    || o.driver_version != self.manifest.version
                    || !matches!(
                        o.result,
                        AudioOutcome::Installed
                            | AudioOutcome::VerifyFailed
                            | AudioOutcome::MoveFailed
                    )
                {
                    return Err(NativeError::Invalid);
                }
                (o.result, o.at_unix_ms)
            };
            let now = self.clock.unix_ms()?;
            if Some(identity(&file)) == attempt.prior
                || now < attempt.start_ms
                || at < attempt.start_ms.saturating_sub(2000)
                || at > now.saturating_add(2000)
            {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(outcome)
        })();
        match result {
            Ok(outcome) => {
                attempt.facts.state = PackageState::Outcome(outcome);
                attempt.facts.error = None;
            }
            Err(error) => {
                attempt.facts.state = PackageState::Unknown;
                attempt.facts.error = Some(error);
            }
        }
        Ok(())
    }
}
