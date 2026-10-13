//! The fixed install, state, config and temp locations (L2) and the payload file set.

use super::error::{SetupError, SetupResult};
use std::path::{Path, PathBuf};

/// One file of the install directory, in the order setup places them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PayloadFile {
    Agent,
    Ui,
    Ctl,
    Installer,
    Helper,
    DriverInf,
    DriverCat,
    DriverDll,
}

impl PayloadFile {
    pub(crate) const ALL: [Self; 8] = [
        Self::Agent,
        Self::Ui,
        Self::Ctl,
        Self::Installer,
        Self::Helper,
        Self::DriverInf,
        Self::DriverCat,
        Self::DriverDll,
    ];

    /// "crosspane-agent.exe" … r"driver\CrosspaneIdd.dll"; also the path under `payload\`
    /// (`Installer` is never there).
    pub(crate) fn relative(self) -> &'static str {
        match self {
            Self::Agent => "crosspane-agent.exe",
            Self::Ui => "crosspane-ui.exe",
            Self::Ctl => "crosspanectl.exe",
            Self::Installer => "crosspane-installer.exe",
            Self::Helper => "crosspane-elevated-setup.exe",
            Self::DriverInf => r"driver\CrosspaneIdd.inf",
            Self::DriverCat => r"driver\CrosspaneIdd.cat",
            Self::DriverDll => r"driver\CrosspaneIdd.dll",
        }
    }
}

/// Every location setup owns. Nothing outside these is ever touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Paths {
    /// `%LOCALAPPDATA%\Programs\Crosspane`
    pub install: PathBuf,
    /// `%LOCALAPPDATA%\Programs`
    pub programs: PathBuf,
    /// `%LOCALAPPDATA%\Crosspane`
    pub state: PathBuf,
    /// `state\Installer`
    pub installer_state: PathBuf,
    /// `state\backups`
    pub backups: PathBuf,
    /// `state\runtime`
    pub runtime: PathBuf,
    /// `%APPDATA%\Crosspane`
    pub config: PathBuf,
    /// `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Crosspane`
    pub start_menu: PathBuf,
    /// `%TEMP%\Crosspane`
    pub temp: PathBuf,
}

impl Paths {
    /// Known folders / environment, validated like `detect.rs:31-52`.
    pub(crate) fn current() -> SetupResult<Self> {
        Err(SetupError::before("not built"))
    }

    /// Pure: the layout under `local` (`%LOCALAPPDATA%`), `roaming` (`%APPDATA%`) and `temp`.
    pub(crate) fn at(local: &Path, roaming: &Path, temp: &Path) -> SetupResult<Self> {
        let _ = (local, roaming, temp);
        Err(SetupError::before("not built"))
    }

    pub(crate) fn file(&self, file: PayloadFile) -> PathBuf {
        self.install.join(file.relative())
    }

    /// `installer_state\installed.json`
    pub(crate) fn installed_json(&self) -> PathBuf {
        self.installer_state.join("installed.json")
    }
}

/// `<exe dir>\payload` if it is a directory.
pub(crate) fn payload_beside_current_exe() -> Option<PathBuf> {
    None
}

/// Case-insensitive comparison with `\\?\` stripped.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    let _ = (a, b);
    false
}

/// Case-insensitive, `\\?\`-stripped: whether `path` is `dir` or inside it.
pub(crate) fn is_under(path: &Path, dir: &Path) -> bool {
    let _ = (path, dir);
    false
}
