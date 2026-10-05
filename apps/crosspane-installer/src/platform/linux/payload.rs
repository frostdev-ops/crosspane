//! Local development payloads. Archive metadata never grants ownership of an arbitrary path.
use super::native_io::{DeadRuntime, Deadline, LinuxNativeIo, NativeError, SupportProof};
use crate::agent_contract::{
    AgentReply, BackendName, BackendState, DecodedReply, InstallerStatusV1, ObservationSource,
    StartupRecovery, StatusAdmission,
};
use aws_lc_rs::digest::{SHA256, digest};
use crosspane_installer_core::{
    InstallReceipt, MutationOutcome, OperationId, ResourceObservation, ResourceOwnership,
    ResourceReceipt, StepId,
};
use rustix::fs::{self as rfs, AtFlags, Mode, OFlags, RenameFlags};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_MEMBER_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
pub const FILES: [&str; 10] = [
    "bin/crosspane-agent",
    "bin/crosspanectl",
    "bin/crosspane-ui",
    "bin/crosspane-installer",
    "bin/crosspane-tutorial",
    "resources/crosspane-agent.service",
    "resources/crosspane-settings.desktop",
    "resources/crosspane-installer.desktop",
    "resources/crosspane-icon.svg",
    "resources/LICENSE",
];
/// Whole quoted values: unit ExecStart, desktop Exec, or complete unit Environment assignments.
pub const TEMPLATE_FIELDS: [&str; 7] = [
    "{{agent_executable}}",
    "{{xdg_config_environment}}",
    "{{xdg_state_environment}}",
    "{{xdg_runtime_environment}}",
    "{{crosspane_runtime_environment}}",
    "{{settings_executable}}",
    "{{installer_executable}}",
];
/// Expected installed facts for the service verifier; this record grants no mutation authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedResource {
    pub template_sha256: [u8; 32],
    pub rendered_sha256: [u8; 32],
    pub target: PathBuf,
    pub source: ObservationSource,
    pub bytes: Vec<u8>,
}

#[cfg(test)]
mod rendering_tests {
    use super::quoted;
    #[test]
    fn environment_quotes_escape_without_changing_executable_dollars() {
        assert_eq!(
            quoted("XDG_CONFIG_HOME=/scratch/space %\"$\\quote", false, false),
            Ok(r#""XDG_CONFIG_HOME=/scratch/space %%\"$\\quote""#.to_owned())
        );
        assert_eq!(
            quoted("/scratch/dollar$/agent", false, true),
            Ok("\"/scratch/dollar$/agent\"".to_owned())
        );
        assert!(quoted("/scratch/quote\"/agent", false, true).is_err());
    }
}
fn quoted(value: &str, desktop: bool, executable: bool) -> Result<String> {
    if value.chars().any(char::is_control) || value.contains("{{") || value.contains("}}") {
        return Err(PayloadError::Invalid);
    }
    // systemd rejects special decoded executable path characters. Refuse them before planning.
    // Executable-name resolution precedes argv expansion: '$' remains literal in argv[0].
    if executable
        && value
            .chars()
            .any(|c| !c.is_alphanumeric() && !"/._- %$+".contains(c))
    {
        return Err(PayloadError::Invalid);
    }
    let mut result = String::from("\"");
    for c in value.chars() {
        if c == '%' {
            result.push_str("%%");
        } else {
            if matches!(c, '"' | '\\') || (desktop && matches!(c, '$' | '`')) {
                result.push('\\');
            }
            result.push(c);
        }
    }
    result.push('"');
    // Desktop string unescaping precedes Exec argument unquoting.
    Ok(if desktop {
        result.replace('\\', "\\\\")
    } else {
        result
    })
}
fn template(raw: &[u8], index: usize) -> Result<&str> {
    if !raw.is_ascii() {
        return Err(PayloadError::Invalid);
    }
    let text = std::str::from_utf8(raw).map_err(|_| PayloadError::Invalid)?;
    let fields = match index {
        5 => &TEMPLATE_FIELDS[..5],
        6 => &TEMPLATE_FIELDS[5..6],
        _ => &TEMPLATE_FIELDS[6..],
    };
    let (mut section, mut seen, mut commands) = ("", 0u8, 0);
    if text
        .chars()
        .any(|c| (c.is_control() && c != '\n') || c == '%')
    {
        return Err(PayloadError::Invalid);
    }
    for line in text.lines() {
        if line.trim_end().ends_with('\\') {
            return Err(PayloadError::Invalid);
        }
        if line.starts_with('[') {
            section = line;
        }
        let directive = line.split_once('=').map(|(key, _)| key.trim());
        if directive.is_some_and(|key| key.starts_with("Exec") || key == "TryExec") {
            commands += 1;
            let expected = if index == 5 {
                format!("ExecStart={} run", fields[0])
            } else {
                format!("Exec={}", fields[0])
            };
            if line != expected
                || section
                    != if index == 5 {
                        "[Service]"
                    } else {
                        "[Desktop Entry]"
                    }
            {
                return Err(PayloadError::Invalid);
            }
        }
        if line.contains("{{") || line.contains("}}") {
            let position = fields
                .iter()
                .position(|field| {
                    line == if index == 5 && *field != fields[0] {
                        format!("Environment={field}")
                    } else if index == 5 {
                        format!("ExecStart={field} run")
                    } else {
                        format!("Exec={field}")
                    }
                })
                .ok_or(PayloadError::Invalid)?;
            if seen & (1 << position) != 0
                || section
                    != if index == 5 {
                        "[Service]"
                    } else {
                        "[Desktop Entry]"
                    }
            {
                return Err(PayloadError::Invalid);
            }
            seen |= 1 << position;
        } else if directive == Some("Environment") {
            return Err(PayloadError::Invalid);
        }
    }
    if commands != 1 || seen != (1 << fields.len()) - 1 {
        return Err(PayloadError::Invalid);
    }
    Ok(text)
}
/// Payload retirement requires completed recovery and operational mandatory backends. Optional
/// CPU/desktop backends may be missing or blocked; a failed backend always retains every backup.
fn operational(health: &InstallerStatusV1) -> bool {
    use BackendName::*;
    matches!(
        health.startup_recovery,
        StartupRecovery::Restored | StartupRecovery::NothingParked
    ) && health.recovery_pending == 0
        && health
            .backends
            .iter()
            .all(|b| b.state != BackendState::Failed)
        && [
            Keystore, Links, Parking, Windows, Frames, Capture, Keys, Pointer,
        ]
        .iter()
        .all(|name| {
            health
                .backends
                .iter()
                .any(|b| b.name == *name && b.state == BackendState::Ready)
        })
}
const PAYLOAD_STEP: StepId = StepId(47);
type Result<T> = std::result::Result<T, PayloadError>;
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PayloadError {
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error("invalid bounded archive or provenance")]
    Invalid,
    #[error("foreign or modified resource; explicit repair required")]
    Foreign,
    #[error("unfinished operation requires re-detection")]
    Pending,
    #[error("interrupted mutation; inspect before retry")]
    OutcomeUnknown,
}
fn system<T>(value: std::result::Result<T, impl std::fmt::Debug>) -> Result<T> {
    value.map_err(|_| PayloadError::Native(NativeError::Unavailable))
}
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut result = [0; 32];
    result.copy_from_slice(digest(&SHA256, bytes).as_ref());
    result
}
fn hash(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(PayloadError::Invalid);
    }
    let mut result = [0; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let digit = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
        result[index] = digit(pair[0]) * 16 + digit(pair[1]);
    }
    Ok(result)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    X86_64,
    Aarch64,
}
impl Architecture {
    pub fn native() -> Result<Self> {
        match std::env::consts::ARCH {
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Aarch64),
            _ => Err(PayloadError::Invalid),
        }
    }
    fn elf(self, bytes: &[u8]) -> Result<()> {
        if bytes.len() < 64
            || &bytes[..7] != b"\x7fELF\x02\x01\x01"
            || ![2, 3].contains(&u16::from_le_bytes([bytes[16], bytes[17]]))
            || u16::from_le_bytes([bytes[18], bytes[19]])
                != match self {
                    Self::X86_64 => 62,
                    Self::Aarch64 => 183,
                }
            || bytes[20..24] != [1, 0, 0, 0]
            || bytes[52..54] != [64, 0]
        {
            return Err(PayloadError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryProvenance {
    pub name: String,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub name: String,
    pub size: usize,
    pub sha256: String,
    pub features: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub product_version: String,
    pub architecture: Architecture,
    pub source_revision: String,
    pub profile: String,
    pub libraries: Vec<LibraryProvenance>,
    pub members: Vec<Artifact>,
}
fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}
fn octal(bytes: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| PayloadError::Invalid)?
        .trim_matches(['\0', ' ']);
    if text.is_empty() || !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return Err(PayloadError::Invalid);
    }
    usize::from_str_radix(text, 8).map_err(|_| PayloadError::Invalid)
}
/// Validated bytes and provenance are immutable; no member name is used as an extraction path.
#[derive(Debug)]
pub struct Package {
    manifest: Manifest,
    files: BTreeMap<String, Vec<u8>>,
    manifest_hash: [u8; 32],
    archive_hash: [u8; 32],
}
impl Package {
    pub fn read(
        reader: impl Read,
        architecture: Architecture,
        expected_sha256: [u8; 32],
    ) -> Result<Self> {
        let mut bytes = Vec::new();
        system(
            reader
                .take(MAX_ARCHIVE_BYTES as u64 + 1)
                .read_to_end(&mut bytes),
        )?;
        if bytes.len() > MAX_ARCHIVE_BYTES
            || bytes.len() % 512 != 0
            || sha256(&bytes) != expected_sha256
        {
            return Err(PayloadError::Invalid);
        }
        let mut files = BTreeMap::new();
        let mut offset = 0;
        loop {
            let header = bytes
                .get(offset..offset + 512)
                .ok_or(PayloadError::Invalid)?;
            if header.iter().all(|b| *b == 0) {
                if bytes.len() - offset < 1024
                    || bytes.len() - offset > 10240
                    || bytes[offset..].iter().any(|b| *b != 0)
                {
                    return Err(PayloadError::Invalid);
                }
                break;
            }
            let end = header[..100]
                .iter()
                .position(|b| *b == 0)
                .ok_or(PayloadError::Invalid)?;
            let name = std::str::from_utf8(&header[..end]).map_err(|_| PayloadError::Invalid)?;
            if header[end..100].iter().any(|b| *b != 0)
                || (name != "manifest.json" && !FILES.contains(&name))
                || files.len() >= 11
                || files.contains_key(name)
                || header[156] != b'0'
                || &header[257..265] != b"ustar\x0000"
                || header[157..257].iter().any(|b| *b != 0)
                || header[265..500].iter().any(|b| *b != 0)
                || header[500..].iter().any(|b| *b != 0)
                || octal(&header[108..116])? != 0
                || octal(&header[116..124])? != 0
                || octal(&header[136..148])? != 0
            {
                return Err(PayloadError::Invalid);
            }
            let checksum: usize = header
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    if (148..156).contains(&i) {
                        32
                    } else {
                        usize::from(*b)
                    }
                })
                .sum();
            if checksum != octal(&header[148..156])?
                || octal(&header[100..108])?
                    != if name.starts_with("bin/") {
                        0o755
                    } else {
                        0o644
                    }
            {
                return Err(PayloadError::Invalid);
            }
            let size = octal(&header[124..136])?;
            if size == 0
                || size
                    > if name == "manifest.json" || FILES[5..8].contains(&name) {
                        MAX_RECORD_BYTES
                    } else {
                        MAX_MEMBER_BYTES
                    }
            {
                return Err(PayloadError::Invalid);
            }
            offset += 512;
            let data = bytes
                .get(offset..offset + size)
                .ok_or(PayloadError::Invalid)?
                .to_vec();
            let padded = size.div_ceil(512) * 512;
            if bytes
                .get(offset + size..offset + padded)
                .ok_or(PayloadError::Invalid)?
                .iter()
                .any(|b| *b != 0)
            {
                return Err(PayloadError::Invalid);
            }
            offset += padded;
            files.insert(name.to_owned(), data);
        }
        let metadata = files.remove("manifest.json").ok_or(PayloadError::Invalid)?;
        let manifest: Manifest =
            serde_json::from_slice(&metadata).map_err(|_| PayloadError::Invalid)?;
        if manifest.schema_version != 1
            || manifest.architecture != architecture
            || !token(&manifest.product_version)
            || manifest.source_revision.len() != 40
            || !manifest
                .source_revision
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            || !["dev", "release"].contains(&manifest.profile.as_str())
            || manifest.members.len() != 10
            || files.len() != 10
            || manifest.libraries.is_empty()
            || manifest.libraries.len() > 32
        {
            return Err(PayloadError::Invalid);
        }
        let mut names = BTreeMap::new();
        for library in &manifest.libraries {
            if !token(&library.name)
                || !library.name.contains(".so")
                || names.insert(&library.name, ()).is_some()
            {
                return Err(PayloadError::Invalid);
            }
            hash(&library.sha256)?;
        }
        names.clear();
        for member in &manifest.members {
            let data = files.get(&member.name).ok_or(PayloadError::Invalid)?;
            if names.insert(&member.name, ()).is_some()
                || member.size != data.len()
                || hash(&member.sha256)? != sha256(data)
                || member.features.len() > 32
                || member.features.iter().any(|v| !token(v))
                || member.features.windows(2).any(|pair| pair[0] >= pair[1])
                || (member.name == FILES[0] && !member.features.iter().any(|v| v == "video"))
            {
                return Err(PayloadError::Invalid);
            }
            if member.name.starts_with("bin/") {
                architecture.elf(data)?;
            }
        }
        Ok(Self {
            manifest,
            files,
            manifest_hash: sha256(&metadata),
            archive_hash: sha256(&bytes),
        })
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The whole already hash-validated agent member (at most `MAX_MEMBER_BYTES`). Its dependency
    /// metadata (PT_DYNAMIC, DT_STRTAB) can lie anywhere in the image. No archive re-parse.
    pub(crate) fn agent_elf(&self) -> &[u8] {
        self.files.get(FILES[0]).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod elf_prefix_tests {
    use super::*;

    fn package_with_agent(bytes: Vec<u8>) -> Package {
        let mut files = BTreeMap::new();
        files.insert(FILES[0].to_owned(), bytes);
        Package {
            manifest: Manifest {
                schema_version: 1,
                product_version: "0.0.0".into(),
                architecture: Architecture::X86_64,
                source_revision: "0".repeat(40),
                profile: "dev".into(),
                libraries: Vec::new(),
                members: Vec::new(),
            },
            files,
            manifest_hash: [0; 32],
            archive_hash: [0; 32],
        }
    }

    #[test]
    fn agent_elf_is_the_whole_validated_agent_beyond_any_header_prefix() {
        use super::super::native_io::MAX_ELF_PREFIX_BYTES;
        assert_eq!(MAX_ELF_PREFIX_BYTES, MAX_MEMBER_BYTES);
        let agent = vec![0x7f; 4 * 1024 * 1024 + 64];
        let package = package_with_agent(agent.clone());
        assert_eq!(package.agent_elf(), agent.as_slice());
        assert_eq!(
            package_with_agent(b"\x7fELF".to_vec()).agent_elf(),
            b"\x7fELF"
        );
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    old: Option<[u8; 32]>,
    new: [u8; 32],
    template: [u8; 32],
    ownership: ResourceOwnership,
    replacement: Option<FileIdentity>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    Intent,
    Applied,
    Verified,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    receipt: InstallReceipt,
    items: Vec<Item>,
    phase: Phase,
    source: ObservationSource,
    previous_instance: Option<u64>,
    base_generation: Option<[u8; 32]>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileIdentity {
    file: (u64, u64),
    parent: (u64, u64),
    hash: [u8; 32],
    mode: u32,
}
/// Plans are opaque and bound to the selected target, the package and the observed file hashes.
#[derive(Debug)]
pub struct PayloadPlan {
    journal: Journal,
    generation: Option<[u8; 32]>,
    resuming: bool,
    superseding_applied: bool,
    dead_runtime: Option<DeadRuntime>,
}
impl PayloadPlan {
    pub fn receipt(&self) -> &InstallReceipt {
        &self.journal.receipt
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchingFiles {
    Preserve,
    Adopt,
}
/// Interruption injection is admitted only for scratch targets, never selected production paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interruption {
    Planning,
    Locked,
    /// True means intent; phases 0/1/2 mean Intent/Applied/Verified. Actual atomic I/O is 4.7a.
    BeforeJournal(bool, u8),
    AfterJournal(bool, u8),
    BeforeQuarantineRecord,
    AfterQuarantineRecord,
    Intent,
    Staged(usize),
    BackedUp(usize),
    Replaced(usize),
    Outcome,
    Write,
    Chmod,
    FileSync,
    Publish,
    DirectorySync,
    Exchange(usize),
    Quarantine,
    Unlink,
    Retired(usize),
}
type ScratchHook = Arc<dyn Fn(Interruption) -> Result<()> + Send + Sync>;
pub struct PayloadInstaller {
    io: Arc<LinuxNativeIo>,
    paths: Vec<PathBuf>,
    state: PathBuf,
    interruption: Option<Interruption>,
    hook: Option<ScratchHook>,
}
impl std::fmt::Debug for PayloadInstaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PayloadInstaller")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}
struct Snapshot {
    bytes: Vec<u8>,
    hash: [u8; 32],
    parent: OwnedFd,
    name: String,
    file: File,
    identity: (u64, u64),
}
impl Snapshot {
    fn admitted_identity(&self, mode: u32) -> Result<FileIdentity> {
        let p = system(rfs::fstat(&self.parent))?;
        Ok(FileIdentity {
            file: self.identity,
            parent: (p.st_dev, p.st_ino),
            hash: self.hash,
            mode,
        })
    }
}
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
impl PayloadInstaller {
    /// Repair authority is additional to current SupportProof, never cleanup authority.
    pub fn apply_after_clean_stop(
        &self,
        proof: &SupportProof,
        package: &Package,
        plan: PayloadPlan,
        clean: &super::removal::CleanStop,
        deadline: &Deadline,
    ) -> Result<InstallReceipt> {
        proof.check(&self.io)?;
        clean.revalidate_for(&self.io, deadline)?;
        self.apply_with(proof, package, plan, Some(clean), deadline)
    }
    pub fn new(io: Arc<LinuxNativeIo>) -> Result<Self> {
        io.validate_target()?;
        let p = io.target().paths();
        let mut paths: Vec<_> = FILES[..5].iter().map(|name| p.prefix.join(name)).collect();
        paths.extend([
            p.config_home.join("systemd/user/crosspane-agent.service"),
            p.data_home.join("applications/crosspane-settings.desktop"),
            p.data_home.join("applications/crosspane-installer.desktop"),
            p.data_home
                .join("icons/hicolor/scalable/apps/crosspane.svg"),
            p.data_home.join("crosspane/LICENSE"),
        ]);
        let state = p.state_home.join("crosspane/installer");
        Ok(Self {
            io,
            paths,
            state,
            interruption: None,
            hook: None,
        })
    }
    pub fn targets(&self) -> &[PathBuf] {
        &self.paths
    }
    fn materialize(&self, package: &Package, index: usize) -> Result<Vec<u8>> {
        let raw = package
            .files
            .get(FILES[index])
            .ok_or(PayloadError::Invalid)?;
        if !(5..8).contains(&index) {
            return Ok(raw.clone());
        }
        let p = self.io.target().paths();
        let environment =
            |name: &str, path: &Path| quoted(&format!("{name}={}", path.display()), false, false);
        let values = match index {
            5 => vec![
                quoted(
                    &self.io.target().agent_path().to_string_lossy(),
                    false,
                    true,
                )?,
                environment("XDG_CONFIG_HOME", &p.config_home)?,
                environment("XDG_STATE_HOME", &p.state_home)?,
                environment("XDG_RUNTIME_DIR", &p.runtime_home)?,
                environment("CROSSPANE_RUNTIME_DIR", self.io.target().runtime_dir())?,
            ],
            6 => vec![quoted(&self.paths[2].to_string_lossy(), true, true)?],
            _ => vec![quoted(&self.paths[3].to_string_lossy(), true, true)?],
        };
        let fields = match index {
            5 => &TEMPLATE_FIELDS[..5],
            6 => &TEMPLATE_FIELDS[5..6],
            _ => &TEMPLATE_FIELDS[6..],
        };
        let mut text = template(raw, index)?.to_owned();
        for (field, value) in fields.iter().zip(values) {
            if text.matches(field).count() != 1 {
                return Err(PayloadError::Invalid);
            }
            text = text.replace(field, &value);
            if text.len() > MAX_RECORD_BYTES {
                return Err(PayloadError::Invalid);
            }
        }
        if text.contains("{{") || text.contains("}}") {
            return Err(PayloadError::Invalid);
        }
        Ok(text.into_bytes())
    }
    /// Re-render only from this admitted target; c compares these bytes/hashes with loaded resources.
    pub fn rendered_resources(&self, package: &Package) -> Result<Vec<RenderedResource>> {
        self.io.validate_target()?;
        (5..8)
            .map(|index| {
                let bytes = self.materialize(package, index)?;
                Ok(RenderedResource {
                    template_sha256: sha256(
                        package
                            .files
                            .get(FILES[index])
                            .ok_or(PayloadError::Invalid)?,
                    ),
                    rendered_sha256: sha256(&bytes),
                    target: self.paths[index].clone(),
                    source: self.io.target().source(),
                    bytes,
                })
            })
            .collect()
    }
    pub fn scratch_interrupt(&mut self, point: Option<Interruption>) -> Result<()> {
        if self.io.target().source() != ObservationSource::Demo {
            return Err(PayloadError::Foreign);
        }
        self.interruption = point;
        Ok(())
    }
    /// Faults and synchronized interleavings are permitted only within an admitted scratch target.
    pub fn scratch_hook(&mut self, hook: Option<ScratchHook>) -> Result<()> {
        if self.io.target().source() != ObservationSource::Demo {
            return Err(PayloadError::Foreign);
        }
        self.hook = hook;
        Ok(())
    }
    fn interrupt(&self, point: Interruption) -> Result<()> {
        if let Some(hook) = &self.hook {
            hook(point)?;
        }
        if self.interruption == Some(point) {
            Err(PayloadError::OutcomeUnknown)
        } else {
            Ok(())
        }
    }
    // Every parent, including parents of newly-created directories, comes from the frozen seam.
    fn parent(
        &self,
        proof: &SupportProof,
        path: &Path,
        create: bool,
    ) -> Result<Option<(OwnedFd, String)>> {
        let home = &self.io.target().paths().home;
        let relative = path.strip_prefix(home).map_err(|_| PayloadError::Foreign)?;
        let mut current = home.clone();
        for part in relative.parent().ok_or(PayloadError::Foreign)?.components() {
            current.push(part);
            let (parent, name) = self.io.parent_for_mutation(proof, &current)?;
            match rfs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(s)
                    if s.st_uid == self.io.target().paths().uid
                        && s.st_mode & 0o170022 == 0o040000 => {}
                Ok(_) => return Err(PayloadError::Foreign),
                Err(rustix::io::Errno::NOENT) if create => {
                    proof.check(&self.io)?;
                    system(rfs::mkdirat(&parent, &name, Mode::RWXU))?;
                    rfs::fsync(&parent).map_err(|_| PayloadError::OutcomeUnknown)?;
                }
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(_) => return Err(PayloadError::Foreign),
            }
        }
        Ok(Some(self.io.parent_for_mutation(proof, path)?))
    }
    fn snapshot(
        &self,
        proof: &SupportProof,
        path: &Path,
        mode: u32,
        limit: usize,
    ) -> Result<Option<Snapshot>> {
        let Some((parent, name)) = self.parent(proof, path, false)? else {
            return Ok(None);
        };
        let fd = match rfs::openat(
            &parent,
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(_) => return Err(PayloadError::Foreign),
        };
        let s = system(rfs::fstat(&fd))?;
        if s.st_uid != self.io.target().paths().uid
            || s.st_mode & 0o177777 != (0o100000 | mode)
            || s.st_nlink != 1
            || s.st_size < 0
            || s.st_size as u64 > limit as u64
        {
            return Err(PayloadError::Foreign);
        }
        let mut bytes = Vec::new();
        let mut file = File::from(fd);
        system((&mut file).take(limit as u64 + 1).read_to_end(&mut bytes))?;
        if bytes.len() > limit {
            return Err(PayloadError::Foreign);
        }
        Ok(Some(Snapshot {
            hash: sha256(&bytes),
            bytes,
            parent,
            name,
            file,
            identity: (s.st_dev, s.st_ino),
        }))
    }
    fn mode(index: usize) -> u32 {
        if index < 5 { 0o755 } else { 0o644 }
    }
    fn sibling(&self, index: usize, op: OperationId, previous: bool) -> PathBuf {
        self.paths[index].with_file_name(format!(
            ".crosspane-{}-{}-{index}",
            if previous { "previous" } else { "stage" },
            op.0
        ))
    }
    fn write_new(&self, proof: &SupportProof, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        let (parent, name) = self
            .parent(proof, path, true)?
            .ok_or(PayloadError::Foreign)?;
        proof.check(&self.io)?;
        let temporary = format!(
            "{name}.partial-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        );
        let mut file = File::from(system(rfs::openat(
            &parent,
            &temporary,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        ))?);
        let result = (|| {
            self.file_operation(&mut file, Interruption::Write, Some(bytes), mode)?;
            self.file_operation(&mut file, Interruption::Chmod, None, mode)?;
            self.file_operation(&mut file, Interruption::FileSync, None, mode)?;
            system(file.seek(SeekFrom::Start(0)))?;
            let mut checked = Vec::new();
            system(
                (&mut file)
                    .take(bytes.len() as u64 + 1)
                    .read_to_end(&mut checked),
            )?;
            if checked != bytes {
                return Err(PayloadError::OutcomeUnknown);
            }
            let admitted = system(rfs::fstat(&file))?;
            proof.check(&self.io)?;
            self.interrupt(Interruption::Publish)?;
            proof.check(&self.io)?;
            system(file.seek(SeekFrom::Start(0)))?;
            let mut current = Vec::new();
            system(
                (&mut file)
                    .take(bytes.len() as u64 + 1)
                    .read_to_end(&mut current),
            )?;
            if current != bytes {
                return Err(PayloadError::Foreign);
            }
            let candidate = system(rfs::statat(&parent, &temporary, AtFlags::SYMLINK_NOFOLLOW))?;
            if (candidate.st_dev, candidate.st_ino) != (admitted.st_dev, admitted.st_ino)
                || candidate.st_mode & 0o177777 != (0o100000 | mode)
                || candidate.st_nlink != 1
                || candidate.st_uid != self.io.target().paths().uid
            {
                return Err(PayloadError::Foreign);
            }
            system(rfs::renameat_with(
                &parent,
                &temporary,
                &parent,
                &name,
                RenameFlags::NOREPLACE,
            ))?;
            let published = system(rfs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW))?;
            if (published.st_dev, published.st_ino) != (admitted.st_dev, admitted.st_ino) {
                return Err(PayloadError::OutcomeUnknown);
            }
            self.sync_parent(&parent)
        })();
        // An unpublished partial never has a recovery name. Only our retained inode is removable.
        if rfs::statat(&parent, &temporary, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|s| {
            rfs::fstat(&file).is_ok_and(|f| (s.st_dev, s.st_ino) == (f.st_dev, f.st_ino))
        }) {
            let _ = rfs::unlinkat(&parent, &temporary, AtFlags::empty());
        }
        result
    }
    fn file_operation(
        &self,
        file: &mut File,
        operation: Interruption,
        bytes: Option<&[u8]>,
        mode: u32,
    ) -> Result<()> {
        self.interrupt(operation)?;
        match operation {
            Interruption::Write => system(file.write_all(bytes.ok_or(PayloadError::Invalid)?)),
            Interruption::Chmod => system(rfs::fchmod(file, Mode::from_bits_truncate(mode))),
            Interruption::FileSync => system(file.sync_all()),
            _ => Err(PayloadError::Invalid),
        }
    }
    fn sync_parent(&self, parent: &OwnedFd) -> Result<()> {
        self.interrupt(Interruption::DirectorySync)?;
        rfs::fsync(parent).map_err(|_| PayloadError::OutcomeUnknown)
    }
    fn generation(&self, proof: &SupportProof) -> Result<Option<[u8; 32]>> {
        Ok(self
            .snapshot(
                proof,
                &self.state.join("payload-outcome.json"),
                0o600,
                MAX_RECORD_BYTES,
            )?
            .map(|s| s.hash))
    }
    fn save(&self, proof: &SupportProof, journal: &Journal, intent: bool) -> Result<()> {
        let path = self.state.join(if intent {
            "payload-intent.json"
        } else {
            "payload-outcome.json"
        });
        self.parent(proof, &path, true)?
            .ok_or(PayloadError::Foreign)?;
        let bytes = serde_json::to_vec(journal).map_err(|_| PayloadError::Invalid)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(PayloadError::Invalid);
        }
        let phase = match journal.phase {
            Phase::Intent => 0,
            Phase::Applied => 1,
            Phase::Verified => 2,
        };
        self.interrupt(Interruption::BeforeJournal(intent, phase))?;
        self.io.atomic_write(proof, &path, &bytes)?;
        self.interrupt(Interruption::AfterJournal(intent, phase))
    }
    fn load(&self, proof: &SupportProof, intent: bool) -> Result<Option<Journal>> {
        let path = self.state.join(if intent {
            "payload-intent.json"
        } else {
            "payload-outcome.json"
        });
        let Some(snapshot) = self.snapshot(proof, &path, 0o600, MAX_RECORD_BYTES)? else {
            return Ok(None);
        };
        let j: Journal =
            serde_json::from_slice(&snapshot.bytes).map_err(|_| PayloadError::Foreign)?;
        if j.receipt.schema_version != 1
            || j.receipt.operation_id.0 == 0
            || j.items.len() != 10
            || j.receipt.resources.len() != 10
            || j.source != self.io.target().source()
            || j.receipt.resources.iter().enumerate().any(|(i, r)| {
                let item = &j.items[i];
                r.resource_id != FILES[i]
                    || Path::new(&r.resolved_path) != self.paths[i]
                    || r.ownership != item.ownership
                    || item
                        .replacement
                        .is_some_and(|p| p.hash != item.new || p.mode != Self::mode(i))
            })
        {
            return Err(PayloadError::Foreign);
        }
        Ok(Some(j))
    }
    fn bind(&self, journal: &Journal, package: &Package) -> Result<()> {
        if package.manifest.architecture != Architecture::native()?
            || journal.source != self.io.target().source()
            || journal.receipt.product_version != package.manifest.product_version
            || journal.receipt.manifest_sha256 != package.manifest_hash
            || journal.receipt.payload_sha256 != package.archive_hash
            || journal.items.len() != 10
            || journal.receipt.resources.len() != 10
            || journal.receipt.resources.iter().enumerate().any(|(i, r)| {
                r.resource_id != FILES[i] || Path::new(&r.resolved_path) != self.paths[i]
            })
        {
            return Err(PayloadError::Foreign);
        }
        for (i, item) in journal.items.iter().enumerate() {
            if item.new != sha256(&self.materialize(package, i)?)
                || item.template
                    != sha256(package.files.get(FILES[i]).ok_or(PayloadError::Invalid)?)
            {
                return Err(PayloadError::Foreign);
            }
        }
        Ok(())
    }
    fn fresh_absence(&self) -> Result<()> {
        self.io
            .dead_runtime()
            .map(|_| ())
            .map_err(|_| PayloadError::Foreign)
    }
    fn current_instance(&self, deadline: &Deadline) -> Result<Option<u64>> {
        // A completed publication need not have started the agent. Child metadata cannot
        // prove absence when its parent is missing, so observe the exact private root first.
        if self.io.metadata(self.io.target().runtime_dir())?.is_none() {
            self.fresh_absence()?;
            return Ok(None);
        }
        if self
            .io
            .metadata(&self.io.target().runtime_dir().join("bootstrap.json"))?
            .is_some()
        {
            if self.io.dead_runtime().is_ok_and(|state| state.is_some()) {
                return Ok(None);
            }
            Ok(Some(self.io.bootstrap(deadline)?.0.instance_id))
        } else {
            self.fresh_absence()?;
            Ok(None)
        }
    }
    fn observations(
        &self,
        proof: &SupportProof,
        package: &Package,
        choice: MatchingFiles,
    ) -> Result<(Vec<Item>, Vec<ResourceReceipt>)> {
        proof.check(&self.io)?;
        self.io.validate_target()?;
        if package.manifest.architecture != Architecture::native()? {
            return Err(PayloadError::Invalid);
        }
        let previous = self.load(proof, false)?;
        let (mut items, mut resources) = (Vec::new(), Vec::new());
        for (i, path) in self.paths.iter().enumerate() {
            let current = self.snapshot(proof, path, Self::mode(i), MAX_MEMBER_BYTES)?;
            let old = current.as_ref().map(|s| s.hash);
            let new = sha256(&self.materialize(package, i)?);
            let known = previous
                .as_ref()
                .filter(|p| {
                    matches!(p.phase, Phase::Applied | Phase::Verified)
                        && old == Some(p.items[i].new)
                        && p.receipt.resources[i].after == ResourceObservation::Matching
                })
                .map(|p| p.items[i].ownership);
            let ownership = if old.is_none() || known == Some(ResourceOwnership::Created) {
                ResourceOwnership::Created
            } else if old == Some(new)
                && (known == Some(ResourceOwnership::Adopted) || choice == MatchingFiles::Adopt)
            {
                ResourceOwnership::Adopted
            } else {
                ResourceOwnership::Foreign
            };
            let before = match old {
                None => ResourceObservation::Absent,
                Some(value) if value == new => ResourceObservation::Matching,
                _ => ResourceObservation::Different,
            };
            resources.push(ResourceReceipt {
                resource_id: FILES[i].into(),
                resolved_path: path.to_string_lossy().into_owned(),
                ownership,
                before,
                after: ResourceObservation::Unknown,
                outcome: MutationOutcome::Unknown,
            });
            items.push(Item {
                old,
                new,
                template: sha256(package.files.get(FILES[i]).ok_or(PayloadError::Invalid)?),
                ownership,
                replacement: None,
            });
        }
        if previous.is_none() {
            if items[0].old.is_some() {
                return Err(PayloadError::Foreign);
            }
            self.fresh_absence()?;
        }
        Ok((items, resources))
    }
    pub fn detect(&self, proof: &SupportProof, package: &Package) -> Result<Vec<ResourceReceipt>> {
        Ok(self
            .observations(proof, package, MatchingFiles::Preserve)?
            .1)
    }
    pub fn plan(
        &self,
        proof: &SupportProof,
        package: &Package,
        operation: OperationId,
        matching: MatchingFiles,
    ) -> Result<PayloadPlan> {
        if operation.0 == 0 {
            return Err(PayloadError::Invalid);
        }
        let generation = self.generation(proof)?;
        if self.load(proof, true)?.is_some() {
            return Err(PayloadError::Pending);
        }
        let mut superseding_applied = false;
        if let Some(previous) = self.load(proof, false)? {
            if previous.phase != Phase::Verified {
                if self.bind(&previous, package).is_ok() {
                    return Err(PayloadError::Pending);
                }
                self.settled_applied(proof, &previous)?;
                superseding_applied = true;
            }
            for i in 0..10 {
                for backup in [false, true] {
                    let sibling = self.sibling(i, previous.receipt.operation_id, backup);
                    let retired = sibling.with_file_name(format!(
                        "{}.retired",
                        sibling
                            .file_name()
                            .and_then(|s| s.to_str())
                            .ok_or(PayloadError::Foreign)?
                    ));
                    if self
                        .snapshot(proof, &sibling, Self::mode(i), MAX_MEMBER_BYTES)?
                        .is_some()
                        || self
                            .snapshot(proof, &retired, Self::mode(i), MAX_MEMBER_BYTES)?
                            .is_some()
                    {
                        return Err(PayloadError::Pending);
                    }
                }
            }
        }
        self.interrupt(Interruption::Planning)?;
        let (items, resources) = self.observations(proof, package, matching)?;
        if self.generation(proof)? != generation {
            return Err(PayloadError::Pending);
        }
        if items.iter().any(|item| {
            item.old.is_some()
                && item.old != Some(item.new)
                && item.ownership != ResourceOwnership::Created
        }) {
            return Err(PayloadError::Foreign);
        }
        Ok(PayloadPlan {
            generation,
            resuming: false,
            superseding_applied,
            dead_runtime: if items[0].old.is_none() {
                self.io.dead_runtime()?
            } else {
                None
            },
            journal: Journal {
                receipt: InstallReceipt {
                    schema_version: 1,
                    operation_id: operation,
                    product_version: package.manifest.product_version.clone(),
                    manifest_sha256: package.manifest_hash,
                    payload_sha256: package.archive_hash,
                    resources,
                    unfinished: vec![PAYLOAD_STEP],
                },
                items,
                phase: Phase::Intent,
                source: self.io.target().source(),
                previous_instance: None,
                base_generation: generation,
            },
        })
    }
    /// Re-detects each actual target and recovery sibling; never replays an unknown mutation blindly.
    pub fn resume_plan(&self, proof: &SupportProof, package: &Package) -> Result<PayloadPlan> {
        let generation = self.generation(proof)?;
        let journal = self
            .load(proof, true)?
            .or(self.load(proof, false)?)
            .ok_or(PayloadError::Pending)?;
        self.bind(&journal, package)?;
        self.recheck(proof, &journal, true)?;
        if self.generation(proof)? != generation {
            return Err(PayloadError::Pending);
        }
        Ok(PayloadPlan {
            dead_runtime: if journal.items[0].old.is_none() {
                // Reading an interrupted journal grants no cleanup authority. Apply revalidates
                // absence or exact dead state under the lock before any publication.
                self.io.dead_runtime().ok().flatten()
            } else {
                None
            },
            journal,
            generation,
            resuming: true,
            superseding_applied: false,
        })
    }
    // An Applied receipt is publication evidence, not agent health. A newer package may
    // supersede it only after every recorded byte/inode is observed and no recovery is displaced.
    fn settled_applied(&self, proof: &SupportProof, journal: &Journal) -> Result<()> {
        if journal.phase != Phase::Applied
            || journal.receipt.unfinished != vec![PAYLOAD_STEP]
            || journal.receipt.resources.iter().any(|row| {
                row.after != ResourceObservation::Matching
                    || row.outcome != MutationOutcome::Unknown
                    || row.ownership == ResourceOwnership::Foreign
            })
        {
            return Err(PayloadError::Pending);
        }
        self.recheck(proof, journal, true)?;
        for (i, item) in journal.items.iter().enumerate() {
            if self
                .snapshot(proof, &self.paths[i], Self::mode(i), MAX_MEMBER_BYTES)?
                .map(|snapshot| snapshot.hash)
                != Some(item.new)
            {
                return Err(PayloadError::Foreign);
            }
        }
        if self
            .snapshot(
                proof,
                &self.state.join("payload-intent.json.retired"),
                0o600,
                MAX_RECORD_BYTES,
            )?
            .is_some()
        {
            return Err(PayloadError::Pending);
        }
        Ok(())
    }
    fn recheck(&self, proof: &SupportProof, journal: &Journal, resuming: bool) -> Result<()> {
        for (i, item) in journal.items.iter().enumerate() {
            let snapshot = self.snapshot(proof, &self.paths[i], Self::mode(i), MAX_MEMBER_BYTES)?;
            let current = snapshot.as_ref().map(|s| s.hash);
            if current != item.old
                && (!resuming
                    || current != Some(item.new)
                    || snapshot
                        .as_ref()
                        .map(|s| s.admitted_identity(Self::mode(i)))
                        .transpose()?
                        != item.replacement)
            {
                return Err(PayloadError::Foreign);
            }
            for previous in [false, true] {
                let existing = self.snapshot(
                    proof,
                    &self.sibling(i, journal.receipt.operation_id, previous),
                    Self::mode(i),
                    MAX_MEMBER_BYTES,
                )?;
                if !previous
                    && current != Some(item.new)
                    && item.replacement.is_some()
                    && existing
                        .as_ref()
                        .map(|s| s.admitted_identity(Self::mode(i)))
                        .transpose()?
                        != item.replacement
                {
                    return Err(PayloadError::Foreign);
                }
                if existing.is_some_and(|s| {
                    if previous {
                        Some(s.hash) != item.old
                    } else {
                        s.hash != item.new
                            && (current != Some(item.new) || Some(s.hash) != item.old)
                    }
                }) {
                    return Err(PayloadError::Foreign);
                }
            }
        }
        Ok(())
    }
    fn remove_known(
        &self,
        proof: &SupportProof,
        path: &Path,
        expected: [u8; 32],
        mode: u32,
    ) -> Result<()> {
        let quarantine = path.with_file_name(format!(
            "{}.retired",
            path.file_name()
                .and_then(|s| s.to_str())
                .ok_or(PayloadError::Foreign)?
        ));
        let original = self.snapshot(proof, path, mode, MAX_MEMBER_BYTES)?;
        if original.is_none()
            && self
                .snapshot(proof, &quarantine, mode, MAX_MEMBER_BYTES)?
                .is_none()
        {
            return Ok(());
        }
        let key: String = sha256(&[path.as_os_str().as_encoded_bytes(), &expected].concat())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let record_path = self.state.join(format!("quarantine-{key}.json"));
        let record =
            if let Some(record) = self.snapshot(proof, &record_path, 0o600, MAX_RECORD_BYTES)? {
                let identity: FileIdentity =
                    serde_json::from_slice(&record.bytes).map_err(|_| PayloadError::Foreign)?;
                if identity.hash != expected || identity.mode != mode {
                    return Err(PayloadError::Foreign);
                }
                Some(identity)
            } else if let Some(s) = &original {
                if s.hash != expected {
                    return Err(PayloadError::Foreign);
                }
                let identity = s.admitted_identity(mode)?;
                let bytes = serde_json::to_vec(&identity).map_err(|_| PayloadError::Invalid)?;
                // This durable identity survives a crash or substitution after the quarantine rename.
                self.interrupt(Interruption::BeforeQuarantineRecord)?;
                self.io.atomic_write(proof, &record_path, &bytes)?;
                self.interrupt(Interruption::AfterQuarantineRecord)?;
                Some(identity)
            } else {
                None
            };
        if let Some(s) = original {
            if s.hash != expected {
                return Err(PayloadError::Foreign);
            }
            let identity = record.as_ref().ok_or(PayloadError::Foreign)?;
            let p = system(rfs::fstat(&s.parent))?;
            if s.identity != identity.file || (p.st_dev, p.st_ino) != identity.parent {
                return Err(PayloadError::Foreign);
            }
            proof.check(&self.io)?;
            self.interrupt(Interruption::Quarantine)?;
            proof.check(&self.io)?;
            let qname = quarantine
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or(PayloadError::Foreign)?;
            system(rfs::renameat_with(
                &s.parent,
                &s.name,
                &s.parent,
                qname,
                RenameFlags::NOREPLACE,
            ))?;
            self.sync_parent(&s.parent)?;
            let moved = system(rfs::statat(&s.parent, qname, AtFlags::SYMLINK_NOFOLLOW))?;
            if (moved.st_dev, moved.st_ino) != s.identity
                || system(rfs::fstat(&s.file))?.st_nlink != 1
            {
                return Err(PayloadError::Foreign);
            }
            // A replaced ancestry cannot transfer deletion authority to a different parent.
            let (parent, _) = self.io.parent_for_mutation(proof, path)?;
            let a = system(rfs::fstat(&parent))?;
            let b = system(rfs::fstat(&s.parent))?;
            if (a.st_dev, a.st_ino) != (b.st_dev, b.st_ino) {
                return Err(PayloadError::Foreign);
            }
        }
        if let Some(s) = self.snapshot(proof, &quarantine, mode, MAX_MEMBER_BYTES)? {
            let identity = record.as_ref().ok_or(PayloadError::Foreign)?;
            let p = system(rfs::fstat(&s.parent))?;
            if s.hash != expected
                || s.identity != identity.file
                || (p.st_dev, p.st_ino) != identity.parent
            {
                return Err(PayloadError::Foreign);
            }
            proof.check(&self.io)?;
            self.interrupt(Interruption::Unlink)?;
            let current = self
                .snapshot(proof, &quarantine, mode, MAX_MEMBER_BYTES)?
                .ok_or(PayloadError::Foreign)?;
            let p = system(rfs::fstat(&current.parent))?;
            if current.identity != identity.file
                || current.hash != expected
                || (p.st_dev, p.st_ino) != identity.parent
            {
                return Err(PayloadError::Foreign);
            }
            proof.check(&self.io)?;
            system(rfs::unlinkat(
                &current.parent,
                &current.name,
                AtFlags::empty(),
            ))?;
            self.sync_parent(&current.parent)?;
        }
        Ok(())
    }
    pub fn apply(
        &self,
        proof: &SupportProof,
        package: &Package,
        plan: PayloadPlan,
        deadline: &Deadline,
    ) -> Result<InstallReceipt> {
        self.apply_with(proof, package, plan, None, deadline)
    }
    fn apply_with(
        &self,
        proof: &SupportProof,
        package: &Package,
        plan: PayloadPlan,
        clean: Option<&super::removal::CleanStop>,
        deadline: &Deadline,
    ) -> Result<InstallReceipt> {
        deadline.check()?;
        if let Some(clean) = clean {
            proof.check(&self.io)?;
            clean.revalidate_for(&self.io, deadline)?;
        }
        self.bind(&plan.journal, package)?;
        self.recheck(proof, &plan.journal, plan.resuming)?;
        if plan.journal.items[0].old.is_none() {
            self.fresh_absence()?;
        }
        let lockpath = self.state.join("install.lock");
        self.parent(proof, &lockpath, true)?
            .ok_or(PayloadError::Foreign)?;
        let lock = self.io.install_lease(proof)?;
        if self.generation(proof)? != plan.generation {
            return Err(PayloadError::Pending);
        }
        self.interrupt(Interruption::Locked)?;
        let outcome = self.load(proof, false)?;
        if plan.superseding_applied {
            self.settled_applied(proof, outcome.as_ref().ok_or(PayloadError::Pending)?)?;
        }
        if outcome.as_ref().is_some_and(|outcome| {
            if !plan.resuming {
                (outcome.phase != Phase::Verified && !plan.superseding_applied)
                    || outcome.receipt.operation_id == plan.journal.receipt.operation_id
            } else {
                outcome.receipt.operation_id != plan.journal.receipt.operation_id
                    && (outcome.phase != Phase::Verified
                        || plan.generation != plan.journal.base_generation)
            }
        }) || (plan.resuming && outcome.is_none() && plan.journal.base_generation.is_some())
            || self.generation(proof)? != plan.generation
        {
            return Err(PayloadError::Pending);
        }
        self.recheck(proof, &plan.journal, plan.resuming)?;
        if plan.journal.items[0].old.is_none() {
            if self.io.dead_runtime()? != plan.dead_runtime {
                return Err(PayloadError::Foreign);
            }
            if let Some(dead) = &plan.dead_runtime {
                self.io.clean_dead_runtime(proof, dead, &lock)?;
            }
            self.fresh_absence()?;
        }
        let mut journal = plan.journal;
        if let Some(clean) = clean {
            clean.revalidate_for(&self.io, deadline)?;
            if plan.resuming && journal.previous_instance != Some(clean.instance_id()) {
                return Err(PayloadError::Foreign);
            }
            // Even matching bytes need a new recovered instance after this clean stop.
            journal.previous_instance = Some(clean.instance_id());
        }
        if let Some(existing) = self.load(proof, true)? {
            if !plan.resuming
                || existing.receipt.operation_id != journal.receipt.operation_id
                || existing.receipt.payload_sha256 != journal.receipt.payload_sha256
            {
                return Err(PayloadError::Pending);
            }
            self.bind(&existing, package)?;
            if clean.is_some_and(|clean| existing.previous_instance != Some(clean.instance_id())) {
                return Err(PayloadError::Foreign);
            }
            journal = existing;
        } else if journal.phase == Phase::Intent {
            self.save(proof, &journal, true)?;
        }
        let result = (|| {
            self.interrupt(Interruption::Intent)?;
            for i in 0..journal.items.len() {
                let item = journal.items[i].clone();
                deadline.check()?;
                if let Some(clean) = clean {
                    clean.revalidate_for(&self.io, deadline)?;
                }
                let current =
                    self.snapshot(proof, &self.paths[i], Self::mode(i), MAX_MEMBER_BYTES)?;
                if current.as_ref().map(|s| s.hash) == Some(item.new) {
                    if item.old != Some(item.new)
                        && (!plan.resuming
                            || current
                                .as_ref()
                                .map(|s| s.admitted_identity(Self::mode(i)))
                                .transpose()?
                                != item.replacement)
                    {
                        return Err(PayloadError::Foreign);
                    }
                    continue;
                }
                if current.as_ref().map(|s| s.hash) != item.old
                    || item.ownership != ResourceOwnership::Created
                {
                    return Err(PayloadError::Foreign);
                }
                let stage = self.sibling(i, journal.receipt.operation_id, false);
                if self
                    .snapshot(proof, &stage, Self::mode(i), MAX_MEMBER_BYTES)?
                    .is_none()
                {
                    self.write_new(proof, &stage, &self.materialize(package, i)?, Self::mode(i))?;
                }
                self.interrupt(Interruption::Staged(i))?;
                if let Some(old) = current {
                    let backup = self.sibling(i, journal.receipt.operation_id, true);
                    if self
                        .snapshot(proof, &backup, Self::mode(i), MAX_MEMBER_BYTES)?
                        .is_none()
                    {
                        self.write_new(proof, &backup, &old.bytes, Self::mode(i))?;
                    }
                }
                self.interrupt(Interruption::BackedUp(i))?;
                let staged = self
                    .snapshot(proof, &stage, Self::mode(i), MAX_MEMBER_BYTES)?
                    .ok_or(PayloadError::Foreign)?;
                if staged.hash != item.new {
                    return Err(PayloadError::Foreign);
                }
                if self
                    .snapshot(proof, &self.paths[i], Self::mode(i), MAX_MEMBER_BYTES)?
                    .map(|s| s.hash)
                    != item.old
                {
                    return Err(PayloadError::Foreign);
                }
                let (parent, target) = self.io.parent_for_mutation(proof, &self.paths[i])?;
                let (stage_parent, source) = self.io.parent_for_mutation(proof, &stage)?;
                let a = system(rfs::fstat(&parent))?;
                let b = system(rfs::fstat(&stage_parent))?;
                let replacement = staged.admitted_identity(Self::mode(i))?;
                if (a.st_dev, a.st_ino) != (b.st_dev, b.st_ino)
                    || replacement.parent != (b.st_dev, b.st_ino)
                    || item.replacement.is_some_and(|bound| bound != replacement)
                {
                    return Err(PayloadError::Foreign);
                }
                deadline.check()?;
                proof.check(&self.io)?;
                if i == 0 && item.old.is_some() && clean.is_none() {
                    journal.previous_instance = self.current_instance(deadline)?;
                }
                // Only this staged inode in this parent may become our replacement on resume.
                journal.items[i].replacement.get_or_insert(replacement);
                self.save(proof, &journal, true)?;
                deadline.check()?;
                proof.check(&self.io)?;
                self.interrupt(Interruption::Exchange(i))?;
                deadline.check()?;
                proof.check(&self.io)?;
                if self
                    .snapshot(proof, &stage, Self::mode(i), MAX_MEMBER_BYTES)?
                    .map(|s| s.admitted_identity(Self::mode(i)))
                    .transpose()?
                    != journal.items[i].replacement
                {
                    return Err(PayloadError::Foreign);
                }
                deadline.check()?;
                proof.check(&self.io)?;
                if let Some(clean) = clean {
                    clean.revalidate_for(&self.io, deadline)?;
                }
                // Linux has no rename-by-fd: same-UID substitution after this check is outside
                // the threat model (4.12a). The post-rename identity check retains recovery bytes.
                system(rfs::renameat_with(
                    &stage_parent,
                    &source,
                    &parent,
                    &target,
                    if item.old.is_some() {
                        RenameFlags::EXCHANGE
                    } else {
                        RenameFlags::NOREPLACE
                    },
                ))?;
                self.sync_parent(&parent)?;
                if item.old.is_some()
                    && self
                        .snapshot(proof, &stage, Self::mode(i), MAX_MEMBER_BYTES)?
                        .map(|s| s.hash)
                        != item.old
                {
                    return Err(PayloadError::OutcomeUnknown);
                }
                if self
                    .snapshot(proof, &self.paths[i], Self::mode(i), MAX_MEMBER_BYTES)?
                    .map(|s| s.admitted_identity(Self::mode(i)))
                    .transpose()?
                    != journal.items[i].replacement
                {
                    return Err(PayloadError::OutcomeUnknown);
                }
                self.interrupt(Interruption::Replaced(i))?;
            }
            journal.phase = Phase::Applied;
            for r in &mut journal.receipt.resources {
                r.after = ResourceObservation::Matching;
            }
            self.save(proof, &journal, false)?;
            self.interrupt(Interruption::Outcome)?;
            let path = self.state.join("payload-intent.json");
            if let Some(intent) = self.snapshot(proof, &path, 0o600, MAX_RECORD_BYTES)? {
                self.remove_known(proof, &path, intent.hash, 0o600)?;
            } else {
                let retired = path.with_file_name("payload-intent.json.retired");
                if let Some(intent) = self.snapshot(proof, &retired, 0o600, MAX_RECORD_BYTES)? {
                    let old: Journal =
                        serde_json::from_slice(&intent.bytes).map_err(|_| PayloadError::Foreign)?;
                    self.bind(&old, package)?;
                    if old.receipt.operation_id != journal.receipt.operation_id {
                        return Err(PayloadError::Foreign);
                    }
                    self.remove_known(proof, &path, intent.hash, 0o600)?;
                }
            }
            Ok(journal.receipt)
        })();
        result.map_err(|_| PayloadError::OutcomeUnknown)
    }
    /// A matched, fresh native Status reply verifies payload identity, not global readiness.
    pub fn verify(
        &self,
        proof: &SupportProof,
        package: &Package,
        expected_reply_id: u64,
        now_ms: u64,
        reply: &AgentReply,
        deadline: &Deadline,
    ) -> Result<InstallReceipt> {
        deadline.check()?;
        proof.check(&self.io)?;
        let _lock = self.io.lock(proof, &self.state.join("install.lock"))?;
        if self.load(proof, true)?.is_some() {
            return Err(PayloadError::Pending);
        }
        let mut journal = self.load(proof, false)?.ok_or(PayloadError::Pending)?;
        self.bind(&journal, package)?;
        if journal.phase == Phase::Intent {
            return Err(PayloadError::Pending);
        }
        let Ok(DecodedReply::Status(StatusAdmission::Supported(health))) = &reply.result else {
            return Err(PayloadError::Pending);
        };
        if !operational(health.installer()) {
            return Err(PayloadError::Pending);
        }
        if expected_reply_id == 0
            || reply.id != expected_reply_id
            || reply.source != self.io.target().source()
            || now_ms < reply.observed_at_ms
            || now_ms - reply.observed_at_ms > 5000
            || health.installer().build.version != package.manifest.product_version
            || journal.previous_instance == Some(health.installer().instance.id)
            || package
                .manifest
                .members
                .iter()
                .find(|m| m.name == FILES[0])
                .is_none_or(|m| m.features != health.installer().build.features)
        {
            return Err(PayloadError::Foreign);
        }
        let (bootstrap, identity) = self.io.bootstrap(deadline)?;
        self.io
            .admit_instance(&health.installer().instance, &bootstrap, &identity)?;
        for (i, item) in journal.items.iter().enumerate() {
            let current = self.snapshot(proof, &self.paths[i], Self::mode(i), MAX_MEMBER_BYTES)?;
            if current.as_ref().map(|s| s.hash) != Some(item.new)
                || (item.old != Some(item.new)
                    && current
                        .as_ref()
                        .map(|s| s.admitted_identity(Self::mode(i)))
                        .transpose()?
                        != item.replacement)
            {
                return Err(PayloadError::Foreign);
            }
        }
        if self.io.bootstrap(deadline)? != (bootstrap, identity) {
            return Err(PayloadError::Foreign);
        }
        journal.phase = Phase::Verified;
        journal.receipt.unfinished.clear();
        for resource in &mut journal.receipt.resources {
            resource.outcome = MutationOutcome::Verified;
        }
        self.save(proof, &journal, false)?;
        for (i, item) in journal.items.iter().enumerate() {
            if let Some(old) = item.old.filter(|old| *old != item.new) {
                deadline.check()?;
                self.remove_known(
                    proof,
                    &self.sibling(i, journal.receipt.operation_id, true),
                    old,
                    Self::mode(i),
                )?;
                self.remove_known(
                    proof,
                    &self.sibling(i, journal.receipt.operation_id, false),
                    old,
                    Self::mode(i),
                )?;
            }
            // An unused new stage can remain after an interrupted attempt followed by matching state.
            self.remove_known(
                proof,
                &self.sibling(i, journal.receipt.operation_id, false),
                item.new,
                Self::mode(i),
            )?;
            self.interrupt(Interruption::Retired(i))?;
        }
        Ok(journal.receipt)
    }
}
