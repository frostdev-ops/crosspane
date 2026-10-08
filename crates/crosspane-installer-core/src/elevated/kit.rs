//! Pinned inventory of the elevated setup kit files (WP-W4.1c2 C4). OS-free: the installer
//! parses the inventory JSON and hashes the files; this module decides whether the inventory is
//! admissible and whether a staged file matches its pin.
use super::driver::{DRIVER_BINARY, DRIVER_CATALOG, DRIVER_INF};
use super::{DRIVER_DIRECTORY, ElevatedError, HELPER_IMAGE};

pub const KIT_SCHEMA: u32 = 1;
pub const MAX_KIT_INVENTORY_BYTES: usize = 16 * 1024;
pub const MAX_KIT_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// One file of the elevated setup kit. The serde names are the inventory's `file` values.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum KitFile {
    Helper,
    DriverInf,
    DriverCatalog,
    DriverBinary,
}

impl KitFile {
    pub const ALL: [Self; 4] = [
        Self::Helper,
        Self::DriverInf,
        Self::DriverCatalog,
        Self::DriverBinary,
    ];

    /// The path components under the kit's install root: `[HELPER_IMAGE]` for the helper, and
    /// `[DRIVER_DIRECTORY, <file name>]` for each driver file.
    pub fn components(self) -> &'static [&'static str] {
        match self {
            Self::Helper => &[HELPER_IMAGE],
            Self::DriverInf => &[DRIVER_DIRECTORY, DRIVER_INF],
            Self::DriverCatalog => &[DRIVER_DIRECTORY, DRIVER_CATALOG],
            Self::DriverBinary => &[DRIVER_DIRECTORY, DRIVER_BINARY],
        }
    }
}

/// The inventory document as parsed by the installer (`schema_version` and `files`). `Debug` is
/// derived for the crate's `missing_debug_implementations` lint.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KitDocument {
    pub schema_version: u32,
    pub files: Vec<KitDocumentFile>,
}

/// One inventory entry. `sha256` is the text as written; `KitManifest::admit` parses it.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KitDocumentFile {
    pub file: KitFile,
    pub size: u64,
    pub sha256: String,
}

/// The pinned size and digest of one kit file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KitPin {
    pub file: KitFile,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// The admitted inventory: exactly one pin per `KitFile`, in `KitFile::ALL` order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KitManifest {
    pins: [KitPin; 4],
}

impl KitManifest {
    /// Schema 1; exactly the four files, each once; size in `1..=MAX_KIT_FILE_BYTES`; `sha256`
    /// through `parse_sha256`. Anything else is `Err(Kit)`.
    pub fn admit(document: KitDocument) -> Result<Self, ElevatedError> {
        if document.schema_version != KIT_SCHEMA {
            return Err(ElevatedError::Kit);
        }
        let mut slots: [Option<KitPin>; 4] = [None; 4];
        for entry in &document.files {
            let index = slot(entry.file);
            if slots[index].is_some() || !(1..=MAX_KIT_FILE_BYTES).contains(&entry.size) {
                return Err(ElevatedError::Kit);
            }
            let sha256 = parse_sha256(&entry.sha256).ok_or(ElevatedError::Kit)?;
            slots[index] = Some(KitPin {
                file: entry.file,
                size: entry.size,
                sha256,
            });
        }
        // A missing file leaves its slot empty. Every slot filled means every file appeared once,
        // which also rules out extra entries.
        let [helper, driver_inf, driver_catalog, driver_binary] = slots;
        Ok(Self {
            pins: [
                helper.ok_or(ElevatedError::Kit)?,
                driver_inf.ok_or(ElevatedError::Kit)?,
                driver_catalog.ok_or(ElevatedError::Kit)?,
                driver_binary.ok_or(ElevatedError::Kit)?,
            ],
        })
    }

    pub fn pin(&self, file: KitFile) -> &KitPin {
        &self.pins[slot(file)]
    }

    /// True when a staged file of `file` has exactly this size and digest.
    pub fn matches(&self, file: KitFile, size: u64, sha256: &[u8; 32]) -> bool {
        let pin = self.pin(file);
        pin.size == size && pin.sha256 == *sha256
    }
}

/// The index of `file` in `KitFile::ALL`, which is the order of `KitManifest::pins`.
fn slot(file: KitFile) -> usize {
    match file {
        KitFile::Helper => 0,
        KitFile::DriverInf => 1,
        KitFile::DriverCatalog => 2,
        KitFile::DriverBinary => 3,
    }
}

/// Exactly 64 lowercase hex digits, not all zero.
pub fn parse_sha256(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let (pairs, []) = bytes.as_chunks::<2>() else {
        return None;
    };
    let mut digest = [0_u8; 32];
    for (out, [high, low]) in digest.iter_mut().zip(pairs) {
        *out = (lower_nibble(*high)? << 4) | lower_nibble(*low)?;
    }
    if digest.iter().all(|byte| *byte == 0) {
        return None;
    }
    Some(digest)
}

const fn lower_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
