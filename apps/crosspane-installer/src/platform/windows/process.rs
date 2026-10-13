//! Enumerate, terminate and wait for processes by image path (L5 step 3).

use super::error::note_io;
use super::paths::{is_under, same_path};
use super::setup_log::setup_note;
use std::{
    ffi::OsString,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_INVALID_PARAMETER, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
        },
    },
};

/// The longest NT path in UTF-16 units: the size of the image-name buffer.
const IMAGE_UNITS: usize = 32768;

/// One running process whose image lives in a directory setup owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid: u32,
    pub image: PathBuf,
}

/// Toolhelp snapshot + `QueryFullProcessImageNameW`; excludes `GetCurrentProcessId()`; skips
/// processes that cannot be opened.
pub(crate) fn under(dirs: &[PathBuf]) -> Vec<Proc> {
    let mut found = Vec::new();
    if dirs.is_empty() {
        return found;
    }
    // SAFETY: plain flag and id arguments; the result is a new snapshot handle or an invalid one.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE || raw.is_null() {
        note_io("process snapshot", &std::io::Error::last_os_error());
        return found;
    }
    // SAFETY: a valid snapshot handle was just returned and nothing else owns it.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let own = std::process::id();
    let mut buffer = vec![0u16; IMAGE_UNITS];
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `snapshot` is a live Toolhelp snapshot, and `entry` is a writable PROCESSENTRY32W
    // whose dwSize is set.
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) } != 0;
    while more {
        let pid = entry.th32ProcessID;
        // 0 is the idle process and 4 is the System process; neither has a user-visible image.
        if pid != 0
            && pid != 4
            && pid != own
            && let Ok(process) = open(PROCESS_QUERY_LIMITED_INFORMATION, pid)
            && let Some(image) = image_of(&process, &mut buffer)
            && dirs.iter().any(|dir| is_under(&image, dir))
        {
            found.push(Proc { pid, image });
        }
        // SAFETY: the same live snapshot and writable entry as above.
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    found
}

/// `TerminateProcess` each, wait (total `wait`), and return the ones still alive.
pub(crate) fn terminate(procs: &[Proc], wait: Duration) -> Vec<Proc> {
    let deadline = Instant::now() + wait;
    let own = std::process::id();
    let access = PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
    let mut alive = Vec::new();
    let mut pending = Vec::new();
    let mut buffer = vec![0u16; IMAGE_UNITS];
    for proc in procs {
        if proc.pid == own {
            // Setup never ends itself; it is reported as still running.
            alive.push(proc.clone());
            continue;
        }
        let process = match open(access, proc.pid) {
            Ok(process) => process,
            // No such process any more.
            Err(ERROR_INVALID_PARAMETER) => continue,
            Err(code) => {
                setup_note!(
                    "stop process {}: could not open it, os error {code}",
                    proc.pid
                );
                alive.push(proc.clone());
                continue;
            }
        };
        if has_exited(&process) {
            continue;
        }
        // The id may have been handed to another program since `under` looked: only the same
        // image is ended.
        match image_of(&process, &mut buffer) {
            Some(image) if same_path(&image, &proc.image) => {}
            Some(_) => {
                setup_note!(
                    "stop process {}: the id now belongs to another program, left alone",
                    proc.pid
                );
                continue;
            }
            None => {
                if has_exited(&process) {
                    continue;
                }
                setup_note!(
                    "stop process {}: could not confirm which program it is, left alone",
                    proc.pid
                );
                alive.push(proc.clone());
                continue;
            }
        }
        // SAFETY: `process` is a live handle opened with PROCESS_TERMINATE; the exit code is a plain value.
        if unsafe { TerminateProcess(process.as_raw_handle(), 1) } == 0 {
            let error = std::io::Error::last_os_error();
            if has_exited(&process) {
                continue;
            }
            note_io(&format!("stop process {}", proc.pid), &error);
            alive.push(proc.clone());
            continue;
        }
        pending.push((proc, process));
    }
    for (proc, process) in pending {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !wait_on(&process, remaining) {
            alive.push(proc.clone());
        }
    }
    alive
}

/// Whether `pid` has exited within `wait`.
pub(crate) fn wait_for_exit(pid: u32, wait: Duration) -> bool {
    match open(PROCESS_SYNCHRONIZE, pid) {
        Ok(process) => wait_on(&process, wait),
        // No such process: it is gone.
        Err(ERROR_INVALID_PARAMETER) => true,
        Err(code) => {
            setup_note!("wait for process {pid}: could not open it, os error {code}");
            false
        }
    }
}

/// A new handle on `pid` with `access`, or the Windows error code that stopped it.
fn open(access: u32, pid: u32) -> Result<OwnedHandle, u32> {
    // SAFETY: plain access, inheritance and id arguments; the result is a new handle or null.
    let raw = unsafe { OpenProcess(access, 0, pid) };
    if raw.is_null() {
        let code = std::io::Error::last_os_error().raw_os_error();
        return Err(code.and_then(|code| u32::try_from(code).ok()).unwrap_or(0));
    }
    // SAFETY: OpenProcess returned a new non-null process handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// The full Win32 image path of the process, or `None` when it cannot be read.
fn image_of(process: &OwnedHandle, buffer: &mut [u16]) -> Option<PathBuf> {
    let mut length = u32::try_from(buffer.len()).ok()?;
    // SAFETY: `process` is a live handle with query rights, `buffer` is writable for `length`
    // units, and `length` receives how many were written (without the NUL).
    let read = unsafe {
        QueryFullProcessImageNameW(process.as_raw_handle(), 0, buffer.as_mut_ptr(), &mut length)
    };
    let length = length as usize;
    if read == 0 || length == 0 || length >= buffer.len() {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buffer[..length])))
}

/// Whether the process object is already signalled (the process has ended).
fn has_exited(process: &OwnedHandle) -> bool {
    wait_on(process, Duration::ZERO)
}

/// Waits up to `wait` for the process to end; true once it has.
fn wait_on(process: &OwnedHandle, wait: Duration) -> bool {
    // u32::MAX would mean "forever".
    let millis = u32::try_from(wait.as_millis())
        .unwrap_or(u32::MAX)
        .min(u32::MAX - 1);
    // SAFETY: `process` is a live handle with SYNCHRONIZE rights.
    let state = unsafe { WaitForSingleObject(process.as_raw_handle(), millis) };
    state == WAIT_OBJECT_0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process id that Windows never hands out (ids are multiples of four and far smaller).
    const NO_SUCH_PID: u32 = u32::MAX - 3;

    #[test]
    fn under_a_missing_directory_finds_nothing() {
        let nowhere = PathBuf::from(r"C:\crosspane-process-test-nonexistent");
        assert!(under(&[nowhere]).is_empty());
        assert!(under(&[]).is_empty());
    }

    #[test]
    fn under_never_lists_this_process() {
        let own = std::process::id();
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().to_path_buf();
        assert!(under(&[dir]).iter().all(|proc| proc.pid != own));
    }

    #[test]
    fn a_process_that_does_not_exist_counts_as_gone() {
        assert!(wait_for_exit(NO_SUCH_PID, Duration::ZERO));
        let ghost = Proc {
            pid: NO_SUCH_PID,
            image: PathBuf::from(r"C:\crosspane-process-test-nonexistent\ghost.exe"),
        };
        assert!(terminate(&[ghost], Duration::ZERO).is_empty());
        assert!(terminate(&[], Duration::ZERO).is_empty());
    }
}
