//! Unelevated launcher of the elevated setup helper (WP-W4.1c). It reads the helper's status without
//! elevation, journals the Intent, asks the shell for one `runas` run, waits a bounded time and
//! journals the Outcome. It never kills the elevated child.

use super::native_io::{NativeError, NativeResult};
use crosspane_installer_core::elevated::{
    HELPER_IMAGE, Outcome, RuleScope, Verb, command_line, is_local_drive_path, render_arguments,
    status::{
        Consent, ElevatedJournal, MAX_STATUS_BYTES, StatusReport, intent_entry, outcome_entry,
    },
};
use std::{
    ffi::OsStr,
    io::{self, Read},
    os::windows::{ffi::OsStrExt, fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use windows::{
    Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    },
    core::{HRESULT, PCWSTR},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, WAIT_OBJECT_0},
    Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
    System::Threading::{CREATE_NO_WINDOW, GetExitCodeProcess, WaitForSingleObject},
};

/// Longest the launcher waits for the read-only `status`. A child still running then is killed.
pub const STATUS_WAIT: Duration = Duration::from_secs(30);
/// Longest the launcher waits for the elevated child. A timeout never kills that child.
pub const ELEVATED_WAIT: Duration = Duration::from_secs(600);
const POLL: Duration = Duration::from_millis(25);
/// One byte past the status cap, so an oversized report is visible rather than truncated.
const STATUS_CAP: u64 = MAX_STATUS_BYTES as u64 + 1;
/// `HRESULT_FROM_WIN32(ERROR_CANCELLED)`: the user declined the UAC prompt.
const DECLINED: HRESULT = HRESULT::from_win32(ERROR_CANCELLED);

/// A located helper image. `locate` is the only constructor.
#[derive(Clone, Debug)]
pub struct ElevatedHelper {
    image: PathBuf,
}

/// What the launcher could establish about one mutating run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// The helper exited with a known outcome, and the state after it was read.
    Verified {
        outcome: Outcome,
        after: StatusReport,
    },
    /// The UAC prompt was declined. Nothing ran.
    Declined,
    /// The helper's exit, the state after it, or both are unknown.
    Unverified {
        outcome: Option<Outcome>,
        after: Option<StatusReport>,
    },
    /// A combined verb ran once: each part's own outcome, in run order, and the state after.
    Parts {
        outcomes: Vec<(Verb, Option<Outcome>)>,
        after: Option<StatusReport>,
    },
}

impl ElevatedHelper {
    /// Accepts an absolute local drive path (`is_local_drive_path`) whose leaf is `HELPER_IMAGE`
    /// and that names a regular file, not a symlink or other reparse point.
    pub fn locate(image: PathBuf) -> NativeResult<Self> {
        if !image.is_absolute()
            || image.file_name() != Some(OsStr::new(HELPER_IMAGE))
            || !image.to_str().is_some_and(is_local_drive_path)
        {
            return Err(NativeError::Invalid);
        }
        let metadata = std::fs::symlink_metadata(&image).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                NativeError::Missing
            } else {
                NativeError::Unavailable
            }
        })?;
        let regular = metadata.file_type().is_file()
            && (metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) == 0;
        if !regular {
            return Err(NativeError::Foreign);
        }
        Ok(Self { image })
    }

    /// Runs the helper's read-only `status` unelevated and returns its validated report. The child
    /// is killed and reaped if it runs past `STATUS_WAIT`, and its output is capped.
    pub fn status(&self, scope: Option<&RuleScope>) -> NativeResult<StatusReport> {
        let arguments = render_arguments(&Verb::Status(scope.cloned()));
        let child = Command::new(&self.image)
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|_| NativeError::Unavailable)?;
        let mut child = OwnedChild(child);
        let stdout = child.0.stdout.take().ok_or(NativeError::Unavailable)?;
        let reader = std::thread::Builder::new()
            .name("crosspane-elevated-status".into())
            .spawn(move || read_bounded(stdout))
            .map_err(|_| NativeError::Unavailable)?;
        let started = Instant::now();
        let exited = loop {
            match child.0.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if started.elapsed() < STATUS_WAIT => std::thread::sleep(POLL),
                _ => break None,
            }
        };
        if !child.settle() {
            // The reader is left detached rather than joined: its pipe may still be open.
            return Err(NativeError::Unavailable);
        }
        let bytes = reader
            .join()
            .map_err(|_| NativeError::Unavailable)?
            .map_err(|_| NativeError::Unavailable)?;
        if !exited.is_some_and(|status| status.success()) {
            return Err(NativeError::Unavailable);
        }
        if bytes.len() > MAX_STATUS_BYTES {
            return Err(NativeError::Oversize);
        }
        let report: StatusReport =
            serde_json::from_slice(&bytes).map_err(|_| NativeError::Invalid)?;
        report.validate().map_err(|_| NativeError::Invalid)?;
        Ok(report)
    }

    /// Runs one mutating `verb` elevated, after the user consented to exactly its lines. The Intent
    /// of each part is journaled before anything runs, and the Outcome of each part after the wait.
    /// A combined verb runs all its parts under one elevation. The elevated child is never killed.
    pub fn run(
        &self,
        verb: &Verb,
        consent: &Consent,
        journal: &mut dyn ElevatedJournal,
    ) -> NativeResult<LaunchOutcome> {
        if consent.action() != verb || !verb.mutates() {
            return Err(NativeError::Invalid);
        }
        let parts = verb.parts();
        let before = self.status(verb.scope())?;
        for part in &parts {
            journal
                .record(&intent_entry(part, Some(&before)))
                .map_err(|_| NativeError::Unavailable)?;
        }
        let (exit, declined) = match shell_run_as(&self.image, verb) {
            Launch::Child(process) => (process.wait(ELEVATED_WAIT), false),
            Launch::Declined => (None, true),
            Launch::Unknown => (None, false),
        };
        let after = self.status(verb.scope()).ok();
        // A combined verb's exit code names both parts; a single verb's exit code names itself.
        let outcomes: Vec<Option<Outcome>> = if verb.is_combined() {
            let pair = exit.and_then(Outcome::split_pair_exit_code);
            vec![pair.map(|(first, _)| first), pair.map(|(_, second)| second)]
        } else {
            vec![exit.and_then(Outcome::from_exit_code)]
        };
        for (part, outcome) in parts.iter().zip(&outcomes) {
            journal
                .record(&outcome_entry(part, *outcome, after.as_ref()))
                .map_err(|_| NativeError::OutcomeUnknown)?;
        }
        if declined {
            return Ok(LaunchOutcome::Declined);
        }
        if verb.is_combined() {
            return Ok(LaunchOutcome::Parts {
                outcomes: parts.into_iter().zip(outcomes).collect(),
                after,
            });
        }
        let outcome = exit.and_then(Outcome::from_exit_code);
        Ok(match (outcome, after) {
            (Some(outcome), Some(after)) => LaunchOutcome::Verified { outcome, after },
            (outcome, after) => LaunchOutcome::Unverified { outcome, after },
        })
    }
}

/// What `ShellExecuteExW` left behind. Only `Child` owns a handle.
enum Launch {
    Child(ProcessHandle),
    Declined,
    Unknown,
}

/// Asks the shell for one `runas` run of the helper with the frozen command line.
fn shell_run_as(image: &Path, verb: &Verb) -> Launch {
    let operation = wide("runas");
    let file = wide(image.as_os_str());
    let parameters = wide(command_line(verb));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(operation.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: 0,
        ..Default::default()
    };
    // SAFETY: `info` is a complete SHELLEXECUTEINFOW. Its three string fields point at NUL-terminated
    // UTF-16 buffers owned by this function, which outlive the call. Every other field is zero or
    // was set above.
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) if info.hProcess.0.is_null() => Launch::Unknown,
        Ok(()) => Launch::Child(ProcessHandle(info.hProcess.0)),
        Err(error) if error.code() == DECLINED => Launch::Declined,
        Err(_) => Launch::Unknown,
    }
}

/// The elevated child's process handle. Dropping it closes the handle only; the child keeps running.
struct ProcessHandle(HANDLE);

impl ProcessHandle {
    /// The child's exit code if it exits within `timeout`; `None` on a timeout or a wait error.
    fn wait(&self, timeout: Duration) -> Option<u32> {
        // An overflowing timeout becomes 0, which times out at once and never kills the child.
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(0);
        // SAFETY: `self.0` is the process handle ShellExecuteExW returned. It stays open until
        // `self` is dropped.
        if unsafe { WaitForSingleObject(self.0, millis) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0_u32;
        // SAFETY: as above, and `code` is a valid out-pointer for the duration of the call.
        if unsafe { GetExitCodeProcess(self.0, &mut code) } == 0 {
            return None;
        }
        Some(code)
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: this closes the handle this value owns, exactly once. The child is not terminated.
        unsafe { CloseHandle(self.0) };
    }
}

/// Retains the exact spawned child. `settle` kills it if it still runs, then reaps it.
struct OwnedChild(Child);

impl OwnedChild {
    fn settle(&mut self) -> bool {
        // The child's own handle is used. The process is never looked up by PID.
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        self.0.wait().is_ok()
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}

/// Reads at most `STATUS_CAP` bytes, so an oversized report is visible and memory stays bounded.
fn read_bounded(stdout: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stdout.take(STATUS_CAP).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// NUL-terminated UTF-16 for a Win32 string argument.
fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
