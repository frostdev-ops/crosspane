//! Start menu shortcuts and the Apps & Features entry (L13).

use super::{
    error::{SetupError, SetupResult, note_io},
    paths::{Paths, PayloadFile, same_path},
    setup_log::setup_note,
};
use std::{
    fs, io,
    path::Path,
    thread::{self, JoinHandle},
};
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize, IPersistFile,
            },
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_EXPAND_SZ,
                REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW,
                RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
            },
        },
        UI::Shell::{IShellLinkW, ShellLink},
    },
    core::{Interface, PCWSTR},
};
use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};

pub(crate) const UNINSTALL_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Crosspane";

/// The settings shortcut and the repair-or-uninstall shortcut: file name and plain name.
const LINK_APP: (&str, &str) = ("Crosspane.lnk", "the Crosspane Start menu shortcut");
const LINK_SETUP: (&str, &str) = (
    "Crosspane Setup.lnk",
    "the Crosspane Setup Start menu shortcut",
);
const MAINTENANCE_ARGUMENT: &str = "--maintenance";
const LINKS_FAILED: &str = "Setup couldn't create the Crosspane Start menu shortcuts.";
const ENTRY_FAILED: &str = "Setup couldn't add Crosspane to Apps & Features.";
/// Registry strings longer than this are not ours; `observe` treats them as absent.
const MAX_VALUE_BYTES: u32 = 32 * 1024;

/// Which shell integrations are in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ShellState {
    pub start_menu: bool,
    pub uninstall_entry: bool,
}

/// True only when both links exist and `InstallLocation` is the same path.
pub(crate) fn observe(paths: &Paths) -> ShellState {
    ShellState {
        start_menu: [LINK_APP, LINK_SETUP]
            .iter()
            .all(|(file, _)| paths.start_menu.join(file).is_file()),
        uninstall_entry: entry_present(paths),
    }
}

/// "Crosspane.lnk" → `crosspane-ui.exe`, "Crosspane Setup.lnk" → `crosspane-installer.exe`
/// (`IShellLinkW` + `IPersistFile` on its own MTA scope); HKCU key values per L13,
/// `ModifyPath`/`UninstallString` = `"<install>\crosspane-installer.exe" --maintenance`,
/// `EstimatedSize` (KB, sum of pins).
///
/// Both halves are tried even when the first fails; the first failure is the one returned.
pub(crate) fn ensure(paths: &Paths, version: &str, estimated_kb: u32) -> SetupResult<()> {
    let links = ensure_links(paths);
    let entry = ensure_entry(paths, version, estimated_kb);
    links.and(entry)
}

/// Removes the shortcuts and the key; returns what could not be removed (plain names).
pub(crate) fn remove(paths: &Paths) -> Vec<String> {
    let mut left = Vec::new();
    for (file, name) in [LINK_APP, LINK_SETUP] {
        match fs::remove_file(paths.start_menu.join(file)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                note_io("remove Start menu shortcut", &error);
                left.push(name.to_owned());
            }
        }
    }
    // Only an empty folder goes; anything else in it is not ours to delete.
    match fs::remove_dir(&paths.start_menu) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(error) => {
            note_io("remove Start menu folder", &error);
            left.push("the Crosspane Start menu folder".to_owned());
        }
    }
    let key = wide(UNINSTALL_KEY);
    // SAFETY: `key` is a live NUL-terminated wide string for the whole call; the root is a
    // predefined handle.
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr())) };
    if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND && status != ERROR_PATH_NOT_FOUND {
        note_win32("remove Apps & Features entry", status.0);
        left.push("the Crosspane entry in Apps & Features".to_owned());
    }
    left
}

// ---- Start menu shortcuts ------------------------------------------------------------------

/// One shortcut to write. Every path is plain (no `\\?\` prefix).
#[derive(Clone, Debug, PartialEq, Eq)]
struct LinkSpec {
    /// File name inside the Start menu folder, for the setup log.
    name: &'static str,
    /// Full path of the `.lnk` file.
    link: String,
    target: String,
    /// Empty = none.
    arguments: &'static str,
    description: &'static str,
    working_dir: String,
}

impl LinkSpec {
    /// The icon is the target executable's own, index 0.
    fn icon(&self) -> &str {
        &self.target
    }
}

fn link_specs(paths: &Paths) -> [LinkSpec; 2] {
    let working_dir = plain(&paths.install);
    let link = |file: &str| plain(&paths.start_menu.join(file));
    [
        LinkSpec {
            name: LINK_APP.0,
            link: link(LINK_APP.0),
            target: plain(&paths.file(PayloadFile::Ui)),
            arguments: "",
            description: "Crosspane settings",
            working_dir: working_dir.clone(),
        },
        LinkSpec {
            name: LINK_SETUP.0,
            link: link(LINK_SETUP.0),
            target: plain(&paths.file(PayloadFile::Installer)),
            arguments: MAINTENANCE_ARGUMENT,
            description: "Repair or uninstall Crosspane",
            working_dir,
        },
    ]
}

fn ensure_links(paths: &Paths) -> SetupResult<()> {
    if let Err(error) = fs::create_dir_all(&paths.start_menu) {
        note_io("create Start menu folder", &error);
        return Err(SetupError::after(LINKS_FAILED));
    }
    save_links(link_specs(paths).into()).map_err(|()| SetupError::after(LINKS_FAILED))
}

/// Writes the shortcuts on a dedicated, short-lived STA thread. Never on the caller's thread:
/// the caller may be the GUI thread, which owns its own COM apartment.
fn save_links(specs: Vec<LinkSpec>) -> Result<(), ()> {
    let worker: io::Result<JoinHandle<Result<(), ()>>> = thread::Builder::new()
        .name("crosspane-shell-links".to_owned())
        .spawn(move || {
            // SAFETY: initializes only this new thread's own apartment; balanced by `Apartment`.
            if let Err(error) = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok() {
                setup_note!("shortcuts: com error 0x{:08X}", error.code().0 as u32);
                return Err(());
            }
            // Declared right after the successful initialization and before any interface, so
            // every interface is released before CoUninitialize runs.
            let _apartment = Apartment;
            for spec in &specs {
                if let Err(error) = write_link(spec) {
                    setup_note!(
                        "shortcut {}: com error 0x{:08X}",
                        spec.name,
                        error.code().0 as u32
                    );
                    return Err(());
                }
            }
            Ok(())
        });
    match worker.map(JoinHandle::join) {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            setup_note!("shortcuts: worker panicked");
            Err(())
        }
        Err(error) => {
            note_io("start shortcut thread", &error);
            Err(())
        }
    }
}

/// Balances a successful `CoInitializeEx` on the thread that made it.
#[derive(Debug)]
struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx on this same thread.
        unsafe { CoUninitialize() };
    }
}

fn write_link(spec: &LinkSpec) -> windows::core::Result<()> {
    let link = wide(&spec.link);
    let target = wide(&spec.target);
    let arguments = wide(spec.arguments);
    let description = wide(spec.description);
    let working_dir = wide(&spec.working_dir);
    let icon = wide(spec.icon());
    // SAFETY: this thread is initialized as an STA; every wide string above is NUL-terminated
    // and outlives the calls that borrow it; the interfaces are released on return.
    let shell_link: IShellLinkW =
        unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }?;
    // SAFETY: as above; `shell_link` is a live interface on this apartment.
    unsafe {
        shell_link.SetPath(PCWSTR(target.as_ptr()))?;
        shell_link.SetDescription(PCWSTR(description.as_ptr()))?;
        shell_link.SetWorkingDirectory(PCWSTR(working_dir.as_ptr()))?;
        if !spec.arguments.is_empty() {
            shell_link.SetArguments(PCWSTR(arguments.as_ptr()))?;
        }
        shell_link.SetIconLocation(PCWSTR(icon.as_ptr()), 0)?;
    }
    let file: IPersistFile = shell_link.cast()?;
    // SAFETY: `link` is a live NUL-terminated wide path; `true` remembers the file and
    // overwrites an existing link.
    unsafe { file.Save(PCWSTR(link.as_ptr()), true) }
}

// ---- Apps & Features entry -----------------------------------------------------------------

/// One registry value.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Text(String),
    Dword(u32),
}

impl Value {
    fn kind(&self) -> REG_VALUE_TYPE {
        match self {
            Self::Text(_) => REG_SZ,
            Self::Dword(_) => REG_DWORD,
        }
    }

    /// The exact bytes stored. `REG_SZ` is UTF-16LE including its terminating NUL, and the byte
    /// count the registry is given includes that NUL.
    fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Text(text) => wide(text).into_iter().flat_map(u16::to_le_bytes).collect(),
            Self::Dword(number) => number.to_le_bytes().to_vec(),
        }
    }
}

/// The `Uninstall\Crosspane` value set, in write order. `install_date` is `yyyymmdd`.
fn uninstall_values(
    paths: &Paths,
    version: &str,
    estimated_kb: u32,
    install_date: &str,
) -> Vec<(&'static str, Value)> {
    let text = |value: &str| Value::Text(value.to_owned());
    let maintenance = format!(
        "\"{}\" {MAINTENANCE_ARGUMENT}",
        plain(&paths.file(PayloadFile::Installer))
    );
    vec![
        ("DisplayName", text("Crosspane")),
        ("DisplayVersion", text(version)),
        ("Publisher", text("Crosspane")),
        (
            "DisplayIcon",
            Value::Text(format!("{},0", plain(&paths.file(PayloadFile::Ui)))),
        ),
        ("InstallLocation", Value::Text(plain(&paths.install))),
        ("UninstallString", Value::Text(maintenance.clone())),
        ("ModifyPath", Value::Text(maintenance)),
        ("InstallDate", text(install_date)),
        ("NoModify", Value::Dword(1)),
        ("NoRepair", Value::Dword(0)),
        ("EstimatedSize", Value::Dword(estimated_kb)),
    ]
}

fn ensure_entry(paths: &Paths, version: &str, estimated_kb: u32) -> SetupResult<()> {
    let values = uninstall_values(paths, version, estimated_kb, &local_date());
    let key = Key::create().map_err(|code| {
        note_win32("create Apps & Features entry", code);
        SetupError::after(ENTRY_FAILED)
    })?;
    for (name, value) in &values {
        if let Err(code) = key.set(name, value) {
            note_win32("write Apps & Features entry", code);
            return Err(SetupError::after(ENTRY_FAILED));
        }
    }
    Ok(())
}

/// `InstallLocation` is this install and `UninstallString` runs the installer.
fn entry_present(paths: &Paths) -> bool {
    let Some(key) = Key::open() else {
        return false;
    };
    entry_matches(
        paths,
        key.string("InstallLocation").as_deref(),
        key.string("UninstallString").as_deref(),
    )
}

fn entry_matches(paths: &Paths, location: Option<&str>, uninstall: Option<&str>) -> bool {
    let (Some(location), Some(uninstall)) = (location, uninstall) else {
        return false;
    };
    same_path(Path::new(location), &paths.install)
        && uninstall
            .to_ascii_lowercase()
            .contains(PayloadFile::Installer.relative())
}

/// An open `Uninstall\Crosspane` key, closed on drop.
#[derive(Debug)]
struct Key(HKEY);

impl Key {
    /// Creates or opens the key for writing; the error is the Win32 status.
    fn create() -> Result<Self, u32> {
        let subkey = wide(UNINSTALL_KEY);
        let mut handle = HKEY::default();
        // SAFETY: `subkey` is a live NUL-terminated wide string; `handle` is a valid output slot
        // that is only used when the call succeeds; the root is a predefined handle.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE | KEY_QUERY_VALUE,
                None,
                &mut handle,
                None,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(Self(handle))
        } else {
            Err(status.0)
        }
    }

    /// Opens the existing key read-only; `None` when it is missing or unreadable.
    fn open() -> Option<Self> {
        let subkey = wide(UNINSTALL_KEY);
        let mut handle = HKEY::default();
        // SAFETY: as in `create`.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                None,
                KEY_QUERY_VALUE,
                &mut handle,
            )
        };
        (status == ERROR_SUCCESS).then_some(Self(handle))
    }

    fn set(&self, name: &str, value: &Value) -> Result<(), u32> {
        let name = wide(name);
        let bytes = value.bytes();
        // SAFETY: `self.0` is an open key; `name` is a live NUL-terminated wide string and
        // `bytes` a live buffer whose length is the stored byte count.
        let status = unsafe {
            RegSetValueExW(
                self.0,
                PCWSTR(name.as_ptr()),
                None,
                value.kind(),
                Some(&bytes),
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(status.0)
        }
    }

    /// A `REG_SZ`/`REG_EXPAND_SZ` value up to its first NUL; `None` for anything else.
    fn string(&self, name: &str) -> Option<String> {
        let name = wide(name);
        let mut kind = REG_VALUE_TYPE(0);
        let mut size = 0u32;
        // SAFETY: `self.0` is an open key; `name` is a live NUL-terminated wide string; the
        // output slots are valid and no data buffer is passed (size query).
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                None,
                Some(&mut size),
            )
        };
        if status != ERROR_SUCCESS
            || (kind != REG_SZ && kind != REG_EXPAND_SZ)
            || size == 0
            || size > MAX_VALUE_BYTES
        {
            return None;
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        let mut capacity = u32::try_from(buffer.len() * 2).ok()?;
        // SAFETY: as above, with a live buffer of exactly `capacity` bytes for the data.
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                PCWSTR(name.as_ptr()),
                None,
                None,
                Some(buffer.as_mut_ptr().cast::<u8>()),
                Some(&mut capacity),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let units = (capacity as usize / 2).min(buffer.len());
        let text = &buffer[..units];
        let end = text
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(text.len());
        String::from_utf16(&text[..end]).ok()
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: closes the key this value opened, exactly once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

// ---- helpers -------------------------------------------------------------------------------

/// Logs a Win32 status without any other detail.
fn note_win32(what: &str, code: u32) {
    note_io(what, &io::Error::from_raw_os_error(code as i32));
}

/// The local date as `yyyymmdd`.
fn local_date() -> String {
    let mut now = SYSTEMTIME {
        wYear: 0,
        wMonth: 0,
        wDayOfWeek: 0,
        wDay: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    };
    // SAFETY: `now` is a valid, writable SYSTEMTIME.
    unsafe { GetLocalTime(&mut now) };
    format_date(now.wYear, now.wMonth, now.wDay)
}

fn format_date(year: u16, month: u16, day: u16) -> String {
    format!("{year:04}{month:02}{day:02}")
}

/// The path as text without a `\\?\` (or `\\?\UNC\`) prefix, which links and registry values
/// must not carry.
fn plain(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text.into_owned()
    }
}

/// UTF-16 with a terminating NUL.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn paths() -> Paths {
        let local = PathBuf::from(r"C:\Users\test\AppData\Local");
        Paths {
            install: local.join(r"Programs\Crosspane"),
            programs: local.join("Programs"),
            state: local.join("Crosspane"),
            installer_state: local.join(r"Crosspane\Installer"),
            backups: local.join(r"Crosspane\backups"),
            runtime: local.join(r"Crosspane\runtime"),
            config: PathBuf::from(r"C:\Users\test\AppData\Roaming\Crosspane"),
            start_menu: PathBuf::from(
                r"C:\Users\test\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Crosspane",
            ),
            temp: PathBuf::from(r"C:\Users\test\AppData\Local\Temp\Crosspane"),
        }
    }

    fn value<'a>(values: &'a [(&'static str, Value)], name: &str) -> &'a Value {
        values
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, value)| value)
            .unwrap_or_else(|| panic!("missing value {name}"))
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    #[test]
    fn uninstall_values_follow_the_contract() {
        let values = uninstall_values(&paths(), "0.1.7", 1234, "20261013");
        let install = r"C:\Users\test\AppData\Local\Programs\Crosspane";
        let maintenance = format!(r#""{install}\crosspane-installer.exe" --maintenance"#);
        assert_eq!(value(&values, "DisplayName"), &text("Crosspane"));
        assert_eq!(value(&values, "DisplayVersion"), &text("0.1.7"));
        assert_eq!(value(&values, "Publisher"), &text("Crosspane"));
        assert_eq!(
            value(&values, "DisplayIcon"),
            &text(&format!(r"{install}\crosspane-ui.exe,0"))
        );
        assert_eq!(value(&values, "InstallLocation"), &text(install));
        assert_eq!(value(&values, "UninstallString"), &text(&maintenance));
        assert_eq!(value(&values, "ModifyPath"), &text(&maintenance));
        assert_eq!(value(&values, "InstallDate"), &text("20261013"));
        assert_eq!(value(&values, "NoModify"), &Value::Dword(1));
        assert_eq!(value(&values, "NoRepair"), &Value::Dword(0));
        assert_eq!(value(&values, "EstimatedSize"), &Value::Dword(1234));
        assert_eq!(values.len(), 11);
    }

    #[test]
    fn extended_length_prefixes_are_never_written() {
        let mut prefixed = paths();
        prefixed.install = PathBuf::from(r"\\?\C:\Users\test\AppData\Local\Programs\Crosspane");
        prefixed.start_menu = PathBuf::from(r"\\?\C:\Users\test\Start Menu\Programs\Crosspane");
        for (name, value) in uninstall_values(&prefixed, "1", 1, "20261013") {
            if let Value::Text(text) = value {
                assert!(!text.contains(r"\\?\"), "{name} carries a prefix: {text}");
            }
        }
        for spec in link_specs(&prefixed) {
            for field in [&spec.link, &spec.target, &spec.working_dir] {
                assert!(!field.contains(r"\\?\"), "{field}");
            }
        }
        assert_eq!(plain(Path::new(r"\\?\UNC\host\share\x")), r"\\host\share\x");
        assert_eq!(plain(Path::new(r"C:\plain")), r"C:\plain");
    }

    #[test]
    fn registry_strings_include_their_terminating_nul() {
        let bytes = Value::Text("Ab".to_owned()).bytes();
        assert_eq!(bytes, vec![b'A', 0, b'b', 0, 0, 0]);
        assert_eq!(bytes.len(), ("Ab".len() + 1) * 2);
        assert_eq!(Value::Text(String::new()).bytes(), vec![0, 0]);
        assert_eq!(Value::Dword(0x0102_0304).bytes(), vec![4, 3, 2, 1]);
        assert_eq!(Value::Text(String::new()).kind(), REG_SZ);
        assert_eq!(Value::Dword(1).kind(), REG_DWORD);
        assert_eq!(wide("a"), vec![u16::from(b'a'), 0]);
    }

    #[test]
    fn shortcuts_point_at_the_installed_files() {
        let [app, setup] = link_specs(&paths());
        let install = r"C:\Users\test\AppData\Local\Programs\Crosspane";
        let folder =
            r"C:\Users\test\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Crosspane";
        assert_eq!(app.link, format!(r"{folder}\Crosspane.lnk"));
        assert_eq!(app.target, format!(r"{install}\crosspane-ui.exe"));
        assert_eq!(app.icon(), app.target);
        assert_eq!(app.arguments, "");
        assert_eq!(app.description, "Crosspane settings");
        assert_eq!(app.working_dir, install);
        assert_eq!(setup.link, format!(r"{folder}\Crosspane Setup.lnk"));
        assert_eq!(setup.target, format!(r"{install}\crosspane-installer.exe"));
        assert_eq!(setup.icon(), setup.target);
        assert_eq!(setup.arguments, "--maintenance");
        assert_eq!(setup.description, "Repair or uninstall Crosspane");
        assert_eq!(setup.working_dir, install);
    }

    #[test]
    fn install_date_is_year_month_day() {
        assert_eq!(format_date(2026, 10, 13), "20261013");
        assert_eq!(format_date(2026, 1, 2), "20260102");
    }

    #[test]
    fn the_entry_needs_this_install_and_the_installer() {
        let paths = paths();
        let install = r"C:\Users\test\AppData\Local\Programs\Crosspane";
        let command = format!(r#""{install}\crosspane-installer.exe" --maintenance"#);
        assert!(entry_matches(&paths, Some(install), Some(&command)));
        assert!(entry_matches(
            &paths,
            Some(&install.to_ascii_uppercase()),
            Some(&command.to_ascii_uppercase())
        ));
        assert!(!entry_matches(&paths, None, Some(&command)));
        assert!(!entry_matches(&paths, Some(install), None));
        assert!(!entry_matches(
            &paths,
            Some(r"C:\Somewhere\Else"),
            Some(&command)
        ));
        assert!(!entry_matches(
            &paths,
            Some(install),
            Some(r#""C:\Other\tool.exe" --maintenance"#)
        ));
    }
}
