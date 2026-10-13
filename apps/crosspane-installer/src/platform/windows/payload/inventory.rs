//! Approval is supplied by the executing installer's build, never by inspected payload bytes.
use super::super::error::{NativeError, NativeResult};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_INVENTORY_BYTES: usize = 64 * 1024;
pub(crate) const MAX_IMAGE_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_STAGING_BYTES: u64 = MAX_IMAGE_BYTES;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PayloadRole {
    Installer,
    Agent,
    Ui,
    Ctl,
}
impl PayloadRole {
    pub(crate) fn leaf(self) -> &'static str {
        match self {
            Self::Installer => "crosspane-installer.exe",
            Self::Agent => "crosspane-agent.exe",
            Self::Ui => "crosspane-ui.exe",
            Self::Ctl => "crosspanectl.exe",
        }
    }
}
/// Serializable observations are correlation only. They cannot construct an ApprovedPe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PeFacts {
    pub size: u64,
    pub sha256: [u8; 32],
    pub machine: u16,
    pub subsystem: u16,
    pub version: String,
}
impl PeFacts {
    pub(crate) fn valid(&self) -> bool {
        self.size >= 128
            && self.size <= MAX_IMAGE_BYTES
            && self.sha256 != [0; 32]
            && matches!(self.machine, 0x8664 | 0xaa64)
            && matches!(self.subsystem, 2 | 3)
            && !self.version.is_empty()
            && self.version.len() <= 128
            && self
                .version
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'+'))
    }
}
#[derive(Clone)]
pub(crate) struct ApprovedPe {
    role: PayloadRole,
    facts: PeFacts,
}
impl std::fmt::Debug for ApprovedPe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovedPe")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}
impl ApprovedPe {
    pub(crate) fn size(&self) -> u64 {
        self.facts.size
    }
    pub(crate) fn sha256(&self) -> [u8; 32] {
        self.facts.sha256
    }
    pub(crate) fn version(&self) -> &str {
        &self.facts.version
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    payloads: Vec<ManifestPe>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestPe {
    role: PayloadRole,
    leaf: String,
    size: u64,
    sha256: String,
    machine: u16,
    subsystem: u16,
    version: String,
}
pub(crate) struct ApprovedInventory {
    payloads: Vec<ApprovedPe>,
}
impl std::fmt::Debug for ApprovedInventory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApprovedInventory")
    }
}
impl ApprovedInventory {
    pub(crate) fn embedded() -> NativeResult<Self> {
        Self::from_embedded(option_env!("CROSSPANE_WINDOWS_APPROVED_INVENTORY"))
    }
    fn from_embedded(value: Option<&str>) -> NativeResult<Self> {
        let value = value.ok_or(NativeError::Unsupported)?;
        if value.len() > MAX_INVENTORY_BYTES {
            return Err(NativeError::Unsupported);
        }
        let manifest: Manifest =
            serde_json::from_str(value).map_err(|_| NativeError::Unsupported)?;
        if manifest.schema_version != 1 || manifest.payloads.len() != 3 {
            return Err(NativeError::Unsupported);
        }
        let mut payloads = Vec::with_capacity(3);
        for pin in manifest.payloads {
            if pin.role == PayloadRole::Installer
                || pin.leaf != pin.role.leaf()
                || payloads.iter().any(|old: &ApprovedPe| old.role == pin.role)
            {
                return Err(NativeError::Unsupported);
            }
            let facts = PeFacts {
                size: pin.size,
                sha256: parse_sha(&pin.sha256)?,
                machine: pin.machine,
                subsystem: pin.subsystem,
                version: pin.version,
            };
            if !facts.valid() {
                return Err(NativeError::Unsupported);
            }
            payloads.push(ApprovedPe {
                role: pin.role,
                facts,
            });
        }
        if payloads
            .iter()
            .try_fold(0u64, |sum, pin| sum.checked_add(pin.size()))
            .is_none_or(|sum| sum > MAX_STAGING_BYTES)
        {
            return Err(NativeError::Unsupported);
        }
        let inventory = Self { payloads };
        for role in [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl] {
            inventory.role(role)?;
        }
        Ok(inventory)
    }
    pub(crate) fn role(&self, role: PayloadRole) -> NativeResult<&ApprovedPe> {
        self.payloads
            .iter()
            .find(|pin| pin.role == role)
            .ok_or(NativeError::Unsupported)
    }
}
fn parse_sha(value: &str) -> NativeResult<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(NativeError::Unsupported);
    }
    let mut bytes = [0; 32];
    for (out, pair) in bytes
        .iter_mut()
        .zip(value.as_bytes().as_chunks::<2>().0.iter())
    {
        let digit = |c: u8| if c <= b'9' { c - b'0' } else { c - b'a' + 10 };
        *out = digit(pair[0]) * 16 + digit(pair[1]);
    }
    Ok(bytes)
}
/// Bounded PE header inspection, shared by the payload source checks and the installed-file checks.
pub(crate) fn pe_header(bytes: &[u8], size: u64) -> NativeResult<(u16, u16)> {
    fn u16_at(bytes: &[u8], at: usize) -> NativeResult<u16> {
        Ok(u16::from_le_bytes(
            bytes
                .get(at..at + 2)
                .ok_or(NativeError::Unsupported)?
                .try_into()
                .map_err(|_| NativeError::Unsupported)?,
        ))
    }
    if !(128..=MAX_IMAGE_BYTES).contains(&size) || bytes.get(..2) != Some(b"MZ") {
        return Err(NativeError::Unsupported);
    }
    let pe = u32::from_le_bytes(
        bytes
            .get(60..64)
            .ok_or(NativeError::Unsupported)?
            .try_into()
            .map_err(|_| NativeError::Unsupported)?,
    ) as usize;
    if !(64..=1024 * 1024 - 128).contains(&pe) || bytes.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err(NativeError::Unsupported);
    }
    let machine = u16_at(bytes, pe + 4)?;
    let optional = pe + 24;
    let optional_size = usize::from(u16_at(bytes, pe + 20)?);
    if optional_size < 70
        || optional
            .checked_add(optional_size)
            .is_none_or(|end| end > bytes.len())
        || u16_at(bytes, optional)? != 0x20b
        || !matches!(machine, 0x8664 | 0xaa64)
    {
        return Err(NativeError::Unsupported);
    }
    let subsystem = u16_at(bytes, optional + 68)?;
    if !matches!(subsystem, 2 | 3) {
        return Err(NativeError::Unsupported);
    }
    Ok((machine, subsystem))
}
