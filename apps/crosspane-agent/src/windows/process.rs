//! Process facts and a bounded replacement-process restart.

use anyhow::{Context, Result, bail, ensure};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::SystemInformation::{
    ComputerNamePhysicalDnsHostname, GetComputerNameExW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, TerminateProcess, WaitForSingleObject,
};

const RESTART_PARENT: &str = "CROSSPANE_INTERNAL_RESTART_PARENT";

struct Process(HANDLE);

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: this guard owns the real process handle returned by OpenProcess.
        unsafe { CloseHandle(self.0) };
    }
}

fn creation_time(handle: HANDLE) -> Result<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: handle is live and all four outputs point to writable FILETIME values.
    let status =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    ensure!(
        status != 0,
        "read process creation time: {}",
        std::io::Error::last_os_error()
    );
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

pub(crate) fn started_unix_ms() -> Result<u64> {
    // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle valid in this process.
    let ticks = creation_time(unsafe { GetCurrentProcess() })?;
    ticks
        .checked_sub(116_444_736_000_000_000)
        .map(|ticks| ticks / 10_000)
        .context("process creation time predates the Unix epoch")
}

/// Called before threads or libraries are started. Only our child consumes this handoff.
pub(crate) fn wait_for_restart_parent() -> Result<()> {
    let Some(parent) = std::env::var_os(RESTART_PARENT) else {
        return Ok(());
    };
    // SAFETY: main calls this before tracing, runtimes, handlers or any spawned threads;
    // no other code can be reading or modifying the process environment yet.
    unsafe { std::env::remove_var(RESTART_PARENT) };
    let parent = parent.to_str().context("invalid restart parent handoff")?;
    let (pid, started) = parent
        .split_once(':')
        .context("invalid restart parent handoff")?;
    let pid: u32 = pid.parse().context("invalid restart parent PID")?;
    let started: u64 = started
        .parse()
        .context("invalid restart parent creation time")?;
    ensure!(pid != std::process::id(), "restart cannot wait for itself");
    // SAFETY: query/synchronize access only; pid names the parent, never a termination target.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(87) {
            return Ok(());
        } // Parent has already exited.
        return Err(error).context("open restart parent");
    }
    let handle = Process(handle);
    if creation_time(handle.0)? != started {
        return Ok(());
    } // Recycled PID; original is gone.
    // SAFETY: the guard holds a valid synchronized process handle throughout the bounded wait.
    match unsafe { WaitForSingleObject(handle.0, 5_000) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => bail!("restart parent did not exit within five seconds"),
        _ => Err(std::io::Error::last_os_error()).context("wait for restart parent"),
    }
}

pub(crate) fn restart() -> ! {
    let result = (|| -> Result<()> {
        // SAFETY: the pseudo-handle is borrowed and valid for this process.
        let started = creation_time(unsafe { GetCurrentProcess() })?;
        std::process::Command::new(std::env::current_exe()?)
            .args(std::env::args_os().skip(1))
            .env(RESTART_PARENT, format!("{}:{started}", std::process::id()))
            .spawn()
            .context("spawn replacement agent")?;
        Ok(())
    })();
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            tracing::error!(%error, "could not restart; exiting");
            std::process::exit(1);
        }
    }
}

pub(crate) fn hostname() -> Option<String> {
    let mut name = vec![0u16; 256];
    let mut length = u32::try_from(name.len()).ok()?;
    // SAFETY: the buffer has length elements and length points to its capacity on entry.
    if unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            name.as_mut_ptr(),
            &mut length,
        )
    } == 0
    {
        return None;
    }
    name.truncate(usize::try_from(length).ok()?);
    String::from_utf16(&name)
        .ok()
        .filter(|name| !name.is_empty())
}

/// Fail closed without running potentially stalled driver/runtime exit handlers.
pub(crate) fn exit_without_handlers(status: u32) -> ! {
    // SAFETY: only this process is terminated, using its borrowed pseudo-handle.
    unsafe { TerminateProcess(GetCurrentProcess(), status) };
    // TerminateProcess does not return on success. If it fails, retain the fail-closed fallback.
    std::process::abort();
}
