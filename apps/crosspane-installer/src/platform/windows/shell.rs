//! Start menu shortcuts and the Apps & Features entry (L13).

use super::{
    error::{SetupError, SetupResult},
    paths::Paths,
};

pub(crate) const UNINSTALL_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Crosspane";

/// Which shell integrations are in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ShellState {
    pub start_menu: bool,
    pub uninstall_entry: bool,
}

/// True only when both links exist and `InstallLocation` is the same path.
pub(crate) fn observe(paths: &Paths) -> ShellState {
    let _ = paths;
    ShellState {
        start_menu: false,
        uninstall_entry: false,
    }
}

/// "Crosspane.lnk" → `crosspane-ui.exe`, "Crosspane Setup.lnk" → `crosspane-installer.exe`
/// (`IShellLinkW` + `IPersistFile` on its own MTA scope); HKCU key values per L13,
/// `ModifyPath`/`UninstallString` = `"<install>\crosspane-installer.exe" --maintenance`,
/// `EstimatedSize` (KB, sum of pins).
pub(crate) fn ensure(paths: &Paths, version: &str, estimated_kb: u32) -> SetupResult<()> {
    let _ = (paths, version, estimated_kb);
    Err(SetupError::before("not built"))
}

/// Removes the shortcuts and the key; returns what could not be removed (plain names).
pub(crate) fn remove(paths: &Paths) -> Vec<String> {
    let _ = paths;
    Vec::new()
}
