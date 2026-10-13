//! The SUPPORT step's checks: Windows version, own session, fixed folders, approved inventory,
//! payload presence.

use std::path::Path;

use windows_sys::Win32::System::{
    SystemInformation::{IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64, OSVERSIONINFOW},
    Threading::{GetCurrentProcess, IsWow64Process2},
};

use super::{
    engine::text::NO_INVENTORY, error::NativeError, files::Inventory, identity, paths::Paths,
};
use crate::live::{CheckState, SupportCheck};

/// The first Windows build of Windows 11 24H2.
const MIN_BUILD: u32 = 26100;

/// The refusal when Windows is too old or the PC is not x64.
pub(crate) const WINDOWS_VERSION: &str = "Crosspane needs Windows 11 24H2 or later on an x64 PC.";
/// The refusal when setup is elevated, impersonating or outside the user's own session.
pub(crate) const NOT_OWN_SESSION: &str =
    "Open Crosspane Setup normally in your signed-in Windows session.";
/// The note shown when no `payload` folder sits beside the installer.
const NO_PAYLOAD_NOTE: &str =
    "Repair and uninstall work; installing needs the files beside the downloaded installer.";

/// The verdict plus the checklist rows shown under it.
///
/// `supported` is false when any row is `Failed`, or when the Windows version row or the session
/// row is `Unconfirmed`. `reason` is the first `Failed` text, else the text of the unconfirmed row
/// that blocked support. A reader tells a definite no from an unreadable fact by looking for a
/// `Failed` row in `checks`.
#[derive(Debug)]
pub(crate) struct Support {
    pub supported: bool,
    pub reason: Option<String>,
    pub checks: Vec<SupportCheck>,
}

/// `identity::native::current()` (`NOT_OWN_SESSION`), `Paths::current()`, `Inventory::embedded()`,
/// payload presence (a note only).
pub(crate) fn check(payload: Option<&Path>) -> Support {
    let folders = match Paths::current() {
        Ok(_) => CheckState::Passed(None),
        Err(error) => CheckState::Failed(error.text),
    };
    let inventory = match Inventory::embedded() {
        Ok(_) => CheckState::Passed(None),
        Err(_) => CheckState::Failed(NO_INVENTORY.to_owned()),
    };
    verdict(vec![
        row(
            "Windows version",
            version_state(windows_version(), native_machine()),
            true,
        ),
        row(
            "Your own Windows session",
            session_state(identity::native::current().map(|_| ())),
            true,
        ),
        row("Your folders", folders, false),
        row("Installer files", inventory, false),
        row(
            "Crosspane files beside this installer",
            payload_state(payload),
            false,
        ),
    ])
}

/// One checklist row, and whether an unconfirmed fact in it also blocks support.
fn row(label: &str, state: CheckState, gates: bool) -> (SupportCheck, bool) {
    (SupportCheck::new(label, state), gates)
}

/// Assembles the verdict: any `Failed` row blocks, and an `Unconfirmed` row blocks only when its
/// fact gates support.
fn verdict(rows: Vec<(SupportCheck, bool)>) -> Support {
    let failed = rows.iter().find_map(|(check, _)| match &check.state {
        CheckState::Failed(text) => Some(text.clone()),
        _ => None,
    });
    let reason = failed.or_else(|| {
        rows.iter().find_map(|(check, gates)| match &check.state {
            CheckState::Unconfirmed(text) if *gates => Some(text.clone()),
            _ => None,
        })
    });
    Support {
        supported: reason.is_none(),
        reason,
        checks: rows.into_iter().map(|(check, _)| check).collect(),
    }
}

/// `(major, build)` of the running Windows, or `None` if it can't be read.
fn windows_version() -> Option<(u32, u32)> {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: RtlGetVersion writes only the initialised descriptor, whose size field is set, and
    // does not read the operating system version through a manifest, so it is not shimmed.
    let status = unsafe { RtlGetVersion(&mut info) };
    (status >= 0).then_some((info.dwMajorVersion, info.dwBuildNumber))
}

/// The PC's own processor architecture, even when this process is emulated; `None` if unreadable.
fn native_machine() -> Option<IMAGE_FILE_MACHINE> {
    let mut process: IMAGE_FILE_MACHINE = 0;
    let mut native: IMAGE_FILE_MACHINE = 0;
    // SAFETY: GetCurrentProcess returns a pseudo handle valid for this process, and both outputs
    // are writable locals.
    let known = unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) };
    (known != 0).then_some(native)
}

// `Wdk_System_SystemServices` is not enabled for this crate's `windows-sys`, so the one ntdll
// function needed is declared here, bound the same way `windows-sys` binds its own imports.
#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// Fills `info` with the true Windows version; returns an NTSTATUS (negative on failure).
    fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32;
}

/// The Windows version row from `(major, build)` and the native machine type. A fact that proves
/// the PC is too old, or not x64, is `Failed` even when the other fact is unreadable.
fn version_state(version: Option<(u32, u32)>, machine: Option<IMAGE_FILE_MACHINE>) -> CheckState {
    let too_old =
        version.is_some_and(|(major, build)| major < 10 || (major == 10 && build < MIN_BUILD));
    let not_x64 = machine.is_some_and(|machine| machine != IMAGE_FILE_MACHINE_AMD64);
    if too_old || not_x64 {
        return CheckState::Failed(WINDOWS_VERSION.to_owned());
    }
    match version {
        Some((_, build)) if machine.is_some() => {
            CheckState::Passed(Some(format!("Windows build {build}")))
        }
        _ => CheckState::Unconfirmed("Couldn't read this PC's Windows version.".to_owned()),
    }
}

/// The own-session row from the identity check. Only a token that couldn't be read is
/// unconfirmed; an elevated, impersonating or other-session token is a definite no.
fn session_state(identity: Result<(), NativeError>) -> CheckState {
    match identity {
        Ok(()) => CheckState::Passed(None),
        Err(NativeError::Unavailable) => CheckState::Unconfirmed(
            "Couldn't read which Windows account and session this is.".to_owned(),
        ),
        Err(_) => CheckState::Failed(NOT_OWN_SESSION.to_owned()),
    }
}

/// The payload row: present is passed, absent is only a note (repair and uninstall don't need it).
fn payload_state(payload: Option<&Path>) -> CheckState {
    if payload.is_some_and(Path::is_dir) {
        CheckState::Passed(None)
    } else {
        CheckState::Note(NO_PAYLOAD_NOTE.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD64: IMAGE_FILE_MACHINE = IMAGE_FILE_MACHINE_AMD64;
    const ARM64: IMAGE_FILE_MACHINE = 0xAA64;

    fn failed() -> CheckState {
        CheckState::Failed(WINDOWS_VERSION.to_owned())
    }

    fn unconfirmed(state: &CheckState) -> bool {
        matches!(state, CheckState::Unconfirmed(_))
    }

    #[test]
    fn windows_11_24h2_on_x64_passes() {
        assert_eq!(
            version_state(Some((10, 26100)), Some(AMD64)),
            CheckState::Passed(Some("Windows build 26100".to_owned()))
        );
        assert!(matches!(
            version_state(Some((10, 26200)), Some(AMD64)),
            CheckState::Passed(_)
        ));
        assert!(matches!(
            version_state(Some((11, 1)), Some(AMD64)),
            CheckState::Passed(_)
        ));
    }

    #[test]
    fn an_older_windows_fails() {
        assert_eq!(version_state(Some((10, 26099)), Some(AMD64)), failed());
        assert_eq!(version_state(Some((10, 22631)), Some(AMD64)), failed());
        assert_eq!(version_state(Some((6, 26100)), Some(AMD64)), failed());
    }

    #[test]
    fn a_pc_that_is_not_x64_fails() {
        assert_eq!(version_state(Some((10, 26100)), Some(ARM64)), failed());
        assert_eq!(version_state(Some((10, 26100)), Some(0)), failed());
    }

    #[test]
    fn a_definite_no_wins_over_an_unreadable_fact() {
        assert_eq!(version_state(Some((10, 19045)), None), failed());
        assert_eq!(version_state(None, Some(ARM64)), failed());
    }

    #[test]
    fn unreadable_facts_are_unconfirmed_not_passed() {
        assert!(unconfirmed(&version_state(None, None)));
        assert!(unconfirmed(&version_state(Some((10, 26100)), None)));
        assert!(unconfirmed(&version_state(None, Some(AMD64))));
    }

    #[test]
    fn the_session_row_separates_a_no_from_an_unreadable_token() {
        assert_eq!(session_state(Ok(())), CheckState::Passed(None));
        assert_eq!(
            session_state(Err(NativeError::Unsupported)),
            CheckState::Failed(NOT_OWN_SESSION.to_owned())
        );
        assert!(unconfirmed(&session_state(Err(NativeError::Unavailable))));
    }

    #[test]
    fn a_missing_payload_is_a_note_never_a_failure() {
        assert!(matches!(payload_state(None), CheckState::Note(_)));
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-payload-folder");
        assert!(matches!(payload_state(Some(&missing)), CheckState::Note(_)));
        // The crate folder exists and is a directory, which is all the row looks at.
        let present = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(payload_state(Some(present)), CheckState::Passed(None));
    }

    fn passed() -> CheckState {
        CheckState::Passed(None)
    }

    #[test]
    fn all_rows_passing_is_supported_without_a_reason() {
        let support = verdict(vec![
            row("Windows version", passed(), true),
            row("Your folders", passed(), false),
            row("Payload", CheckState::Note("n".to_owned()), false),
        ]);
        assert!(support.supported);
        assert_eq!(support.reason, None);
        assert_eq!(support.checks.len(), 3);
        assert_eq!(support.checks[0].label, "Windows version");
    }

    #[test]
    fn the_first_failed_row_is_the_reason() {
        let support = verdict(vec![
            row("Windows version", passed(), true),
            row(
                "Your folders",
                CheckState::Failed("first".to_owned()),
                false,
            ),
            row(
                "Installer files",
                CheckState::Failed("second".to_owned()),
                false,
            ),
        ]);
        assert!(!support.supported);
        assert_eq!(support.reason.as_deref(), Some("first"));
    }

    #[test]
    fn a_failed_row_outranks_an_earlier_unconfirmed_one() {
        let support = verdict(vec![
            row(
                "Windows version",
                CheckState::Unconfirmed("unread".to_owned()),
                true,
            ),
            row("Your folders", CheckState::Failed("no".to_owned()), false),
        ]);
        assert!(!support.supported);
        assert_eq!(support.reason.as_deref(), Some("no"));
    }

    #[test]
    fn an_unconfirmed_version_or_session_blocks_support() {
        let support = verdict(vec![
            row("Windows version", passed(), true),
            row(
                "Your own Windows session",
                CheckState::Unconfirmed("unread".to_owned()),
                true,
            ),
            row("Your folders", passed(), false),
        ]);
        assert!(!support.supported);
        assert_eq!(support.reason.as_deref(), Some("unread"));
    }

    #[test]
    fn an_unconfirmed_other_row_does_not_block_support() {
        let support = verdict(vec![
            row("Windows version", passed(), true),
            row(
                "Your folders",
                CheckState::Unconfirmed("unread".to_owned()),
                false,
            ),
        ]);
        assert!(support.supported);
        assert_eq!(support.reason, None);
    }
}
