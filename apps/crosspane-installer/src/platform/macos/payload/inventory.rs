//! Approved inventory, signatures, architecture and exact tree snapshots.
use super::*;

pub const MAX_PAYLOAD_FILES: usize = 128;
pub const MAX_PAYLOAD_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadFile {
    pub path: String,
    pub size: u64,
    pub sha256: [u8; 32],
    pub mode: u32,
    /// None means nonexecutable data. Embedded code has its own approved identity and no grants.
    pub signing: Option<SigningRule>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningRule {
    pub role: PayloadRole,
    pub identifier: String,
    pub designated_requirement: String,
    pub entitlements: BTreeMap<String, bool>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PayloadRole {
    Agent,
    Settings,
    Tutorial,
    Ctl,
    Installer,
    EmbeddedCode,
}
impl SigningRule {
    pub(super) fn native(&self) -> SigningRequirement {
        SigningRequirement {
            role: match self.role {
                PayloadRole::Agent => ArtifactRole::Agent,
                PayloadRole::Settings => ArtifactRole::Settings,
                PayloadRole::Tutorial => ArtifactRole::Tutorial,
                PayloadRole::Ctl => ArtifactRole::Ctl,
                PayloadRole::Installer => ArtifactRole::Installer,
                PayloadRole::EmbeddedCode => ArtifactRole::EmbeddedCode,
            },
            identifier: self.identifier.clone(),
            designated_requirement: self.designated_requirement.clone(),
            entitlements: self.entitlements.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedInventory {
    pub product_version: String,
    pub features: Vec<String>,
    pub files: Vec<PayloadFile>,
}
fn bounded(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}
fn relative(s: &str) -> bool {
    bounded(s, 512)
        && !s.starts_with('/')
        && !s.ends_with('/')
        && !s.contains("//")
        && s.split('/').count() <= 16
        && Path::new(s)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !s.split('/').any(|c| matches!(c, "." | ".."))
}
pub(super) fn hash(bytes: &[u8]) -> [u8; 32] {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
    let mut result = [0; 32];
    result.copy_from_slice(digest.as_ref());
    result
}
impl ApprovedInventory {
    pub(super) fn validate(&mut self) -> NativeResult<()> {
        if !bounded(&self.product_version, 128)
            || self.features.len() > 32
            || self.features.iter().any(|f| !bounded(f, 64))
            || self.files.is_empty()
            || self.files.len() > MAX_PAYLOAD_FILES
        {
            return Err(NativeError::Invalid);
        }
        self.features.sort();
        if self.features.windows(2).any(|w| w[0] == w[1]) {
            return Err(NativeError::Invalid);
        }
        self.files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut roles = BTreeSet::new();
        let mut total = 0u64;
        for (index, entry) in self.files.iter().enumerate() {
            if !relative(&entry.path)
                || (index > 0 && self.files[index - 1].path == entry.path)
                || !(entry.path.starts_with("Crosspane.app/")
                    || entry.path == CTL
                    || entry.path == INSTALLER)
                || entry.size == 0
                || entry.size > MAX_FILE_BYTES as u64
            {
                return Err(NativeError::Invalid);
            }
            total = total.checked_add(entry.size).ok_or(NativeError::Oversize)?;
            if total > MAX_PAYLOAD_BYTES {
                return Err(NativeError::Oversize);
            }
            match &entry.signing {
                None if entry.mode == 0o644
                    && !entry.path.starts_with("Crosspane.app/Contents/MacOS/")
                    && !entry.path.starts_with("Crosspane.app/Contents/Frameworks/") => {}
                Some(rule) if entry.mode == 0o755 => {
                    if !bounded(&rule.identifier, 256)
                        || !bounded(&rule.designated_requirement, 4096)
                        || rule.entitlements.len() > 32
                        || rule.entitlements.keys().any(|s| !bounded(s, 256))
                        || (rule.role != PayloadRole::Agent
                            && rule
                                .entitlements
                                .contains_key("com.apple.security.device.audio-input"))
                    {
                        return Err(NativeError::Invalid);
                    }
                    let expected = match rule.role {
                        PayloadRole::Agent => AGENT,
                        PayloadRole::Settings => "Crosspane.app/Contents/MacOS/crosspane-ui",
                        PayloadRole::Tutorial => "Crosspane.app/Contents/MacOS/crosspane-tutorial",
                        PayloadRole::Ctl => CTL,
                        PayloadRole::Installer => INSTALLER,
                        PayloadRole::EmbeddedCode => {
                            if !entry.path.starts_with("Crosspane.app/Contents/Frameworks/")
                                || !entry.path.ends_with(".dylib")
                                || !rule.entitlements.is_empty()
                            {
                                return Err(NativeError::Invalid);
                            }
                            &entry.path
                        }
                    };
                    if expected != entry.path
                        || (rule.role != PayloadRole::EmbeddedCode && !roles.insert(rule.role))
                        || (rule.role == PayloadRole::Agent && rule.identifier != AGENT_LABEL)
                    {
                        return Err(NativeError::Invalid);
                    }
                }
                _ => return Err(NativeError::Invalid),
            }
        }
        if !self
            .files
            .iter()
            .any(|f| f.path == "Crosspane.app/Contents/Info.plist" && f.signing.is_none())
        {
            return Err(NativeError::Invalid);
        }
        if roles
            != BTreeSet::from([
                PayloadRole::Agent,
                PayloadRole::Settings,
                PayloadRole::Tutorial,
                PayloadRole::Ctl,
                PayloadRole::Installer,
            ])
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn payload_digest(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        for file in &self.files {
            bytes.extend_from_slice(&(file.path.len() as u64).to_le_bytes());
            bytes.extend_from_slice(file.path.as_bytes());
            bytes.extend_from_slice(&file.size.to_le_bytes());
            bytes.extend_from_slice(&file.sha256);
        }
        hash(&bytes)
    }
    pub(super) fn digest(&self) -> NativeResult<[u8; 32]> {
        Ok(hash(
            &serde_json::to_vec(self).map_err(|_| NativeError::Invalid)?,
        ))
    }
}
fn word(bytes: &[u8], at: usize, big: bool) -> NativeResult<u32> {
    let array: [u8; 4] = bytes
        .get(at..at + 4)
        .ok_or(NativeError::Invalid)?
        .try_into()
        .map_err(|_| NativeError::Invalid)?;
    Ok(if big {
        u32::from_be_bytes(array)
    } else {
        u32::from_le_bytes(array)
    })
}
fn thin(bytes: &[u8], embedded: bool) -> NativeResult<()> {
    if bytes.len() < 32
        || word(bytes, 0, false)? != 0xfeedfacf
        || word(bytes, 4, false)? != 0x0100000c
        || word(bytes, 12, false)? != if embedded { 6 } else { 2 }
    {
        return Err(NativeError::Unsupported);
    }
    let count = word(bytes, 16, false)? as usize;
    let end = 32usize
        .checked_add(word(bytes, 20, false)? as usize)
        .ok_or(NativeError::Invalid)?;
    if count > 1024 || end > bytes.len() {
        return Err(NativeError::Invalid);
    }
    let mut position = 32;
    for _ in 0..count {
        let length = word(bytes, position + 4, false)? as usize;
        if length < 8 || !length.is_multiple_of(8) {
            return Err(NativeError::Invalid);
        }
        position = position
            .checked_add(length)
            .filter(|p| *p <= end)
            .ok_or(NativeError::Invalid)?;
    }
    if position != end {
        return Err(NativeError::Invalid);
    }
    Ok(())
}
/// Supports thin arm64 and standard big-endian fat32/fat64 containers, with bounded slices.
fn architecture(bytes: &[u8], embedded: bool) -> NativeResult<()> {
    let magic = word(bytes, 0, true)?;
    if !matches!(magic, 0xcafebabe | 0xcafebabf) {
        return thin(bytes, embedded);
    }
    let count = word(bytes, 4, true)? as usize;
    let stride = if magic == 0xcafebabe { 20 } else { 32 };
    if !(1..=16).contains(&count) || 8 + count * stride > bytes.len() {
        return Err(NativeError::Invalid);
    }
    let mut arm = None;
    let mut ranges = Vec::new();
    for n in 0..count {
        let at = 8 + n * stride;
        let number = |offset| -> NativeResult<u64> {
            let high = word(bytes, offset, true)? as u64;
            Ok(if stride == 20 {
                high
            } else {
                (high << 32) | word(bytes, offset + 4, true)? as u64
            })
        };
        let offset = number(at + 8)?;
        let size = number(at + if stride == 20 { 12 } else { 16 })?;
        let end = offset.checked_add(size).ok_or(NativeError::Invalid)?;
        if offset < (8 + count * stride) as u64
            || size < 32
            || end > bytes.len() as u64
            || ranges.iter().any(|(a, b)| offset < *b && end > *a)
        {
            return Err(NativeError::Invalid);
        }
        ranges.push((offset, end));
        if word(bytes, at, true)? == 0x0100000c {
            if arm.is_some() {
                return Err(NativeError::Invalid);
            }
            arm = Some(&bytes[offset as usize..end as usize]);
        }
    }
    thin(arm.ok_or(NativeError::Unsupported)?, embedded)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Tree {
    pub(super) root: Option<FileIdentity>,
    pub(super) nodes: BTreeMap<String, FileIdentity>,
    pub(super) hashes: BTreeMap<String, [u8; 32]>,
}
pub(super) fn same_object(a: &FileIdentity, b: &FileIdentity) -> bool {
    a.device == b.device && a.inode == b.inode && a.uid == b.uid && a.mode == b.mode
}
pub(super) fn renamed_tree(before: &Tree, after: &Tree) -> bool {
    before.nodes == after.nodes
        && before.hashes == after.hashes
        && match (&before.root, &after.root) {
            (None, None) => true,
            (Some(old), Some(new)) => {
                let mut expected = old.clone();
                expected.changed_ns = new.changed_ns;
                expected == *new
            }
            _ => false,
        }
}
pub(super) fn tree(io: &MacNativeIo, root: &Path, deadline: &Deadline) -> NativeResult<Tree> {
    let mut result = Tree {
        root: io.metadata(root)?,
        nodes: BTreeMap::new(),
        hashes: BTreeMap::new(),
    };
    let Some(identity) = &result.root else {
        return Ok(result);
    };
    let uid = io.target().paths().uid;
    if identity.mode & 0o170000 == 0o100000 {
        identity.regular(uid, false)?;
        result.hashes.insert(
            String::new(),
            hash(&io.read(root, MAX_FILE_BYTES, false, deadline)?),
        );
        return Ok(result);
    }
    let mut pending = vec![(String::new(), identity.clone())];
    let mut total = 0u64;
    while let Some((relative, expected)) = pending.pop() {
        if expected.uid != uid
            || expected.mode & 0o170000 != 0o040000
            || expected.mode & 0o7022 != 0
            || relative.split('/').count() > 16
        {
            return Err(NativeError::Foreign);
        }
        let path = if relative.is_empty() {
            root.to_owned()
        } else {
            root.join(&relative)
        };
        for (name, entry) in io.entries(&path, MAX_PAYLOAD_FILES * 4, deadline)? {
            if result.nodes.len() >= MAX_PAYLOAD_FILES * 4 {
                return Err(NativeError::Oversize);
            }
            let key = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            match entry.mode & 0o170000 {
                0o040000 => pending.push((key.clone(), entry.clone())),
                0o100000 => {
                    entry.regular(uid, false)?;
                    total = total
                        .checked_add(entry.length)
                        .ok_or(NativeError::Oversize)?;
                    if total > MAX_PAYLOAD_BYTES {
                        return Err(NativeError::Oversize);
                    }
                    result.hashes.insert(
                        key.clone(),
                        hash(&io.read(&root.join(&key), MAX_FILE_BYTES, false, deadline)?),
                    );
                }
                _ => return Err(NativeError::Foreign),
            }
            result.nodes.insert(key, entry);
        }
        if io.metadata(&path)? != Some(expected) {
            return Err(NativeError::Foreign);
        }
    }
    if io.metadata(root)? != result.root {
        return Err(NativeError::Foreign);
    }
    Ok(result)
}
pub(super) fn directories(files: impl Iterator<Item = String>) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    for file in files {
        let mut parent = Path::new(&file).parent();
        while let Some(path) = parent.filter(|p| !p.as_os_str().is_empty()) {
            result.insert(path.to_string_lossy().into_owned());
            parent = path.parent();
        }
    }
    result
}

impl MacPayload {
    /// Read-only admission. Must run on a detached worker, as must every method accepting Deadline.
    pub fn admit(
        io: Arc<MacNativeIo>,
        mut approved: ApprovedInventory,
        deadline: &Deadline,
    ) -> NativeResult<Self> {
        approved.validate()?;
        let source = io.target().paths().payload_root.clone();
        let main_rule = approved
            .files
            .iter()
            .find(|f| f.path == AGENT)
            .and_then(|f| f.signing.as_ref())
            .ok_or(NativeError::Invalid)?;
        let main = io.admit_main_signature(&source.join(AGENT), &main_rule.native(), deadline)?;
        let support = io.admit_support(&main, deadline)?;
        let value = Self {
            digest: approved.digest()?,
            io,
            approved,
            main,
            support,
        };
        value.admit_tree(&source, true, deadline)?;
        Ok(value)
    }
    pub fn manifest_sha256(&self) -> [u8; 32] {
        self.digest
    }
    pub(super) fn bundle(&self, root: &Path, deadline: &Deadline) -> NativeResult<()> {
        let command = CommandSpec::new(
            self.io.target(),
            NativeOperation::Signature {
                path: root.to_owned(),
                query: SignatureQuery::Verify,
            },
        )?;
        let output = self.io.execute(&command, None, deadline)?;
        if output.code != Some(0) || !output.stdout.is_empty() || !output.stderr.is_empty() {
            return Err(NativeError::Unsupported);
        }
        Ok(())
    }
    pub(super) fn admit_tree(
        &self,
        root: &Path,
        exact: bool,
        deadline: &Deadline,
    ) -> NativeResult<Tree> {
        let observed = tree(&self.io, root, deadline)?;
        let expected_files: BTreeSet<_> =
            self.approved.files.iter().map(|f| f.path.clone()).collect();
        let expected_dirs = directories(expected_files.iter().cloned());
        let files: BTreeSet<_> = observed.hashes.keys().cloned().collect();
        let dirs: BTreeSet<_> = observed
            .nodes
            .iter()
            .filter(|(_, v)| v.mode & 0o170000 == 0o040000)
            .map(|(k, _)| k.clone())
            .collect();
        if observed.root.is_none() || files != expected_files || dirs != expected_dirs {
            return Err(NativeError::Foreign);
        }
        for file in &self.approved.files {
            let identity = observed.nodes.get(&file.path).ok_or(NativeError::Foreign)?;
            if identity.mode & 0o777 != file.mode
                || (exact
                    && (identity.length != file.size || observed.hashes[&file.path] != file.sha256))
            {
                return Err(NativeError::Foreign);
            }
            if let Some(signing) = &file.signing {
                architecture(
                    &self
                        .io
                        .read(&root.join(&file.path), MAX_FILE_BYTES, false, deadline)?,
                    signing.role == PayloadRole::EmbeddedCode,
                )?;
                self.io.admit_artifact_signature(
                    &root.join(&file.path),
                    &signing.native(),
                    &self.main,
                    deadline,
                )?;
            }
        }
        self.bundle(&root.join(APP), deadline)?;
        if tree(&self.io, root, deadline)? != observed {
            return Err(NativeError::Foreign);
        }
        Ok(observed)
    }
    pub(super) fn installed(&self, deadline: &Deadline) -> NativeResult<(Tree, Tree, bool)> {
        let app = tree(&self.io, &self.io.target().app_path(), deadline)?;
        let ctl = tree(&self.io, &self.ctl(), deadline)?;
        let mut matching = true;
        if app.root.is_some() {
            let expected: BTreeSet<_> = self
                .approved
                .files
                .iter()
                .filter_map(|f| f.path.strip_prefix("Crosspane.app/").map(str::to_owned))
                .collect();
            let dirs = directories(expected.iter().cloned());
            if app.hashes.keys().cloned().collect::<BTreeSet<_>>() != expected
                || app
                    .nodes
                    .iter()
                    .filter(|(_, v)| v.mode & 0o170000 == 0o040000)
                    .map(|(k, _)| k.clone())
                    .collect::<BTreeSet<_>>()
                    != dirs
            {
                return Err(NativeError::Foreign);
            }
            for file in self
                .approved
                .files
                .iter()
                .filter(|f| f.path.starts_with("Crosspane.app/"))
            {
                let rel = file
                    .path
                    .strip_prefix("Crosspane.app/")
                    .ok_or(NativeError::Invalid)?;
                let id = &app.nodes[rel];
                if id.mode & 0o777 != file.mode {
                    return Err(NativeError::Foreign);
                }
                matching &= app.hashes[rel] == file.sha256 && id.length == file.size;
                if let Some(rule) = &file.signing {
                    let path = self.io.target().app_path().join(rel);
                    architecture(
                        &self.io.read(&path, MAX_FILE_BYTES, false, deadline)?,
                        rule.role == PayloadRole::EmbeddedCode,
                    )?;
                    self.io.admit_artifact_signature(
                        &path,
                        &rule.native(),
                        &self.main,
                        deadline,
                    )?;
                }
            }
            self.bundle(&self.io.target().app_path(), deadline)?;
        } else {
            matching = false;
        }
        if let Some(identity) = &ctl.root {
            let file = self
                .approved
                .files
                .iter()
                .find(|f| f.path == CTL)
                .ok_or(NativeError::Invalid)?;
            if identity.mode & 0o170000 != 0o100000 || identity.mode & 0o777 != 0o755 {
                return Err(NativeError::Foreign);
            }
            matching &= ctl.hashes.get("").ok_or(NativeError::Foreign)? == &file.sha256
                && identity.length == file.size;
            architecture(
                &self.io.read(&self.ctl(), MAX_FILE_BYTES, false, deadline)?,
                false,
            )?;
            self.io.admit_artifact_signature(
                &self.ctl(),
                &file.signing.as_ref().ok_or(NativeError::Invalid)?.native(),
                &self.main,
                deadline,
            )?;
        } else {
            matching = false;
        }
        if tree(&self.io, &self.io.target().app_path(), deadline)? != app
            || tree(&self.io, &self.ctl(), deadline)? != ctl
        {
            return Err(NativeError::Foreign);
        }
        Ok((app, ctl, matching))
    }
}
