//! `installed.json` (L1): the only persisted installer state, informational and never gating.

use super::{
    error::{SetupError, SetupResult},
    paths::Paths,
};

/// Written last, after a successful converge.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Installed {
    pub schema: u32,
    pub version: String,
    pub installed_at_unix_ms: u64,
    pub files: std::collections::BTreeMap<String, String>,
}

/// A missing or corrupt file is `None`.
pub(crate) fn read(paths: &Paths) -> Option<Installed> {
    let _ = paths;
    None
}

/// Temp file + rename.
pub(crate) fn write(paths: &Paths, installed: &Installed) -> SetupResult<()> {
    let _ = (paths, installed);
    Err(SetupError::before("not built"))
}
