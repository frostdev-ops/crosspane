//! The fixed install, state, config and temp locations (L2) and the payload file set.

use super::error::{SetupError, SetupResult};
use super::setup_log::setup_note;
use std::{
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{
            FOLDERID_LocalAppData, FOLDERID_RoamingAppData, KF_FLAG_DONT_VERIFY,
            SHGetKnownFolderPath,
        },
    },
    core::GUID,
};

/// Longest base folder accepted, in characters (the same bound `detect.rs` uses).
const MAX_BASE_CHARS: usize = 32760;

const FOLDERS_NOT_FOUND: &str = "Crosspane Setup couldn't find this account's app data folders.";
const FOLDERS_UNUSABLE: &str = "Crosspane Setup can't use this account's folders. They must be on a local drive and have ordinary names.";

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
        let local = known_folder(&FOLDERID_LocalAppData)?;
        let roaming = known_folder(&FOLDERID_RoamingAppData)?;
        Self::at(&local, &roaming, &std::env::temp_dir())
    }

    /// Pure: the layout under `local` (`%LOCALAPPDATA%`), `roaming` (`%APPDATA%`) and `temp`.
    pub(crate) fn at(local: &Path, roaming: &Path, temp: &Path) -> SetupResult<Self> {
        let unusable = || SetupError::before(FOLDERS_UNUSABLE);
        let local = admit_base(local).ok_or_else(unusable)?;
        let roaming = admit_base(roaming).ok_or_else(unusable)?;
        let temp = admit_base(temp).ok_or_else(unusable)?;
        let state = local.join("Crosspane");
        Ok(Self {
            install: local.join(r"Programs\Crosspane"),
            programs: local.join("Programs"),
            installer_state: state.join("Installer"),
            backups: state.join("backups"),
            runtime: state.join("runtime"),
            state,
            config: roaming.join("Crosspane"),
            start_menu: roaming.join(r"Microsoft\Windows\Start Menu\Programs\Crosspane"),
            temp: temp.join("Crosspane"),
        })
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
    let exe = std::env::current_exe().ok()?;
    let payload = exe.parent()?.join("payload");
    payload.is_dir().then_some(payload)
}

/// Case-insensitive comparison with `\\?\` stripped.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    comparable(a) == comparable(b)
}

/// Case-insensitive, `\\?\`-stripped: whether `path` is `dir` or inside it.
pub(crate) fn is_under(path: &Path, dir: &Path) -> bool {
    text_is_under(&comparable(path), &comparable(dir))
}

/// The Shell's path for a known folder of the current user. Never creates the folder.
fn known_folder(id: &GUID) -> SetupResult<PathBuf> {
    // SAFETY: a read-only Shell lookup for the current token; DONT_VERIFY never touches or creates
    // the folder. The returned string is CoTaskMem-owned and freed below.
    let value = match unsafe { SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY, None) } {
        Ok(value) => value,
        Err(error) => {
            setup_note!(
                "known folder lookup failed: hresult {:#010x}",
                error.code().0
            );
            return Err(SetupError::before(FOLDERS_NOT_FOUND));
        }
    };
    if value.is_null() {
        setup_note!("known folder lookup returned no path");
        return Err(SetupError::before(FOLDERS_NOT_FOUND));
    }
    // SAFETY: the Shell returned a NUL-terminated UTF-16 string, which `as_wide` reads up to the NUL.
    let path = PathBuf::from(OsString::from_wide(unsafe { value.as_wide() }));
    // SAFETY: frees exactly the string SHGetKnownFolderPath returned; `path` holds its own copy.
    unsafe {
        CoTaskMemFree(Some(value.0.cast()));
    }
    Ok(path)
}

/// A base folder in stored form, or `None` when it is not an ordinary local-drive folder: absolute
/// with a drive letter, no UNC share, no `.` or `..` component and no empty component. A `\\?\`
/// prefix and one trailing backslash (as `temp_dir()` returns) are dropped; the drive letter is
/// upper-cased. The component rules are those of `FixedPaths::admit` in `detect.rs`.
fn admit_base(path: &Path) -> Option<PathBuf> {
    let text = path.to_str()?;
    // `\\?\UNC\server\share` and `\\server\share` both fail the drive-letter test below.
    let text = text.strip_prefix(r"\\?\").unwrap_or(text);
    let text = text.strip_suffix('\\').unwrap_or(text);
    let bytes = text.as_bytes();
    if bytes.len() <= 3
        || bytes.len() >= MAX_BASE_CHARS
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1..3] != *b":\\"
        || !text[3..].split('\\').all(ordinary_component)
    {
        return None;
    }
    Some(PathBuf::from(format!(
        "{}{}",
        bytes[0].to_ascii_uppercase() as char,
        &text[1..]
    )))
}

fn ordinary_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && !component.ends_with(['.', ' '])
        && !component.contains(['/', ':', '\0'])
}

/// The path as the file system names it when that resolves, else as given; then normalised.
fn comparable(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    normalise(&resolved.to_string_lossy())
}

/// `\\?\` removed (`\\?\UNC\` becomes `\\`), `/` turned into `\`, a trailing `\` dropped (but not
/// from a bare `C:\`), and the result lower-cased.
fn normalise(raw: &str) -> String {
    let text = raw.replace('/', "\\");
    let mut text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text
    };
    while text.len() > 1 && text.ends_with('\\') && !is_drive_root(&text) {
        text.pop();
    }
    text.to_lowercase()
}

fn is_drive_root(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 3 && bytes[1] == b':' && bytes[2] == b'\\'
}

/// `path` is `dir` or starts with `dir` plus a separator (both already normalised).
fn text_is_under(path: &str, dir: &str) -> bool {
    if dir.is_empty() {
        return false;
    }
    if path == dir {
        return true;
    }
    if dir.ends_with('\\') {
        path.starts_with(dir)
    } else {
        path.strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('\\'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: &str = r"C:\Users\Test\AppData\Local";
    const ROAMING: &str = r"C:\Users\Test\AppData\Roaming";
    const TEMP: &str = r"C:\Users\Test\AppData\Local\Temp";
    /// A folder that does not exist, so path comparisons never touch the file system.
    const NOWHERE: &str = r"C:\crosspane-paths-test-nonexistent";

    fn at(local: &str, roaming: &str, temp: &str) -> SetupResult<Paths> {
        Paths::at(Path::new(local), Path::new(roaming), Path::new(temp))
    }

    fn buf(text: &str) -> PathBuf {
        PathBuf::from(text)
    }

    #[test]
    fn at_derives_every_field() {
        let paths = at(LOCAL, ROAMING, TEMP).unwrap();
        assert_eq!(
            paths.install,
            buf(r"C:\Users\Test\AppData\Local\Programs\Crosspane")
        );
        assert_eq!(paths.programs, buf(r"C:\Users\Test\AppData\Local\Programs"));
        assert_eq!(paths.state, buf(r"C:\Users\Test\AppData\Local\Crosspane"));
        assert_eq!(
            paths.installer_state,
            buf(r"C:\Users\Test\AppData\Local\Crosspane\Installer")
        );
        assert_eq!(
            paths.backups,
            buf(r"C:\Users\Test\AppData\Local\Crosspane\backups")
        );
        assert_eq!(
            paths.runtime,
            buf(r"C:\Users\Test\AppData\Local\Crosspane\runtime")
        );
        assert_eq!(
            paths.config,
            buf(r"C:\Users\Test\AppData\Roaming\Crosspane")
        );
        assert_eq!(
            paths.start_menu,
            buf(r"C:\Users\Test\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Crosspane")
        );
        assert_eq!(
            paths.temp,
            buf(r"C:\Users\Test\AppData\Local\Temp\Crosspane")
        );
    }

    #[test]
    fn file_and_installed_json_sit_under_the_derived_folders() {
        let paths = at(LOCAL, ROAMING, TEMP).unwrap();
        assert_eq!(
            paths.file(PayloadFile::Agent),
            buf(r"C:\Users\Test\AppData\Local\Programs\Crosspane\crosspane-agent.exe")
        );
        assert_eq!(
            paths.file(PayloadFile::DriverDll),
            buf(r"C:\Users\Test\AppData\Local\Programs\Crosspane\driver\CrosspaneIdd.dll")
        );
        assert_eq!(
            paths.installed_json(),
            buf(r"C:\Users\Test\AppData\Local\Crosspane\Installer\installed.json")
        );
    }

    #[test]
    fn at_strips_the_verbatim_prefix_and_one_trailing_backslash() {
        let plain = at(LOCAL, ROAMING, TEMP).unwrap();
        let verbatim = at(
            r"\\?\C:\Users\Test\AppData\Local",
            r"\\?\C:\Users\Test\AppData\Roaming",
            r"C:\Users\Test\AppData\Local\Temp\",
        )
        .unwrap();
        assert_eq!(plain, verbatim);
        assert!(!verbatim.install.to_string_lossy().contains('?'));
    }

    #[test]
    fn at_accepts_a_lower_case_drive_letter_and_stores_it_upper_case() {
        let paths = at(r"c:\Users\Test\AppData\Local", ROAMING, TEMP).unwrap();
        assert_eq!(paths.state, buf(r"C:\Users\Test\AppData\Local\Crosspane"));
    }

    #[test]
    fn at_rejects_unusable_bases_in_every_position() {
        let bad = [
            r"Users\Test\AppData\Local",
            r".\Local",
            r"\Users\Test",
            r"C:Users\Test",
            r"C:\",
            r"C:",
            "",
            r"\\server\share\Users",
            r"\\?\UNC\server\share\Users",
            r"\\.\C:\Users\Test",
            r"C:\Users\Test\..\Other",
            r"C:\Users\.\Test",
            r"C:\Users\Test\..",
            r"C:\Users\\Test",
            r"C:\Users\Test\\",
            "C:/Users/Test",
            r"C:\Users\Test.",
            r"C:\Users\Test ",
            r"C:\Users\Te:st",
        ];
        for base in bad {
            for position in 0..3 {
                let mut args = [LOCAL, ROAMING, TEMP];
                args[position] = base;
                let error = at(args[0], args[1], args[2]).unwrap_err();
                assert!(!error.changed, "{base:?} at {position}");
            }
        }
    }

    #[test]
    fn payload_file_set_is_complete_and_ordered() {
        let mut sorted = PayloadFile::ALL;
        sorted.sort();
        assert_eq!(sorted, PayloadFile::ALL);
        assert_eq!(PayloadFile::Agent.relative(), "crosspane-agent.exe");
        assert_eq!(
            PayloadFile::DriverInf.relative(),
            r"driver\CrosspaneIdd.inf"
        );
    }

    #[test]
    fn same_path_ignores_verbatim_prefix_case_slashes_and_trailing_backslash() {
        let plain = buf(&format!(r"{NOWHERE}\Crosspane"));
        assert!(same_path(&plain, &plain));
        assert!(same_path(
            &plain,
            &buf(&format!(r"\\?\{NOWHERE}\Crosspane"))
        ));
        assert!(same_path(
            &plain,
            &buf(&format!(r"{NOWHERE}\CROSSPANE\").to_uppercase())
        ));
        assert!(same_path(
            &plain,
            &buf(&format!("{}/Crosspane", NOWHERE.replace('\\', "/")))
        ));
        assert!(!same_path(&plain, &buf(&format!(r"{NOWHERE}\Crosspane2"))));
        assert!(!same_path(&plain, &buf(NOWHERE)));
    }

    #[test]
    fn normalise_maps_verbatim_unc_onto_plain_unc() {
        // Text only: resolving a share name could make the network lookup a server.
        assert_eq!(
            normalise(r"\\?\UNC\Server\Share\Dir"),
            normalise(r"\\SERVER\share\dir\")
        );
    }

    #[test]
    fn normalise_keeps_a_bare_drive_root() {
        assert_eq!(normalise(r"C:\"), r"c:\");
        assert_eq!(normalise(r"\\?\C:\"), r"c:\");
        assert_eq!(normalise(r"C:\A\B\"), r"c:\a\b");
        assert_eq!(normalise(r"\\?\UNC\Srv\Share\"), r"\\srv\share");
    }

    #[test]
    fn is_under_matches_the_directory_and_its_contents_only() {
        let dir = buf(&format!(r"{NOWHERE}\Crosspane"));
        assert!(is_under(&dir, &dir));
        assert!(is_under(
            &buf(&format!(r"{NOWHERE}\Crosspane\crosspane-agent.exe")),
            &dir
        ));
        assert!(is_under(
            &buf(&format!(r"\\?\{NOWHERE}\CROSSPANE\driver\a.dll")),
            &dir
        ));
        assert!(is_under(
            &buf(&format!(r"{NOWHERE}\Crosspane\x")),
            &buf(&format!(r"\\?\{NOWHERE}\crosspane\"))
        ));
        // A sibling that shares a prefix is not inside.
        assert!(!is_under(
            &buf(&format!(r"{NOWHERE}\Crosspane2\a.exe")),
            &dir
        ));
        assert!(!is_under(&buf(&format!(r"{NOWHERE}\Crosspane2")), &dir));
        // The parent is not inside the child.
        assert!(!is_under(&buf(NOWHERE), &dir));
    }

    #[test]
    fn text_is_under_handles_a_drive_root_and_an_empty_directory() {
        assert!(text_is_under(r"c:\a\b", r"c:\"));
        assert!(text_is_under(r"c:\", r"c:\"));
        assert!(!text_is_under(r"d:\a", r"c:\"));
        assert!(!text_is_under(r"c:\a", ""));
    }

    #[test]
    fn same_path_resolves_existing_paths() {
        // Read-only: the temp directory exists on every machine.
        let temp = std::env::temp_dir();
        let canonical = std::fs::canonicalize(&temp).unwrap();
        assert!(same_path(&temp, &canonical));
        assert!(same_path(
            &temp,
            &PathBuf::from(temp.to_string_lossy().to_uppercase())
        ));
        assert!(is_under(&canonical, &temp));
    }
}
