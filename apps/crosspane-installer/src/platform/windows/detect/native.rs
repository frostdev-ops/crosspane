use super::{Fact, HostFacts, WindowsVersion};
use windows_sys::{
    Wdk::System::SystemServices::RtlGetVersion,
    Win32::System::{
        SystemInformation::{IMAGE_FILE_MACHINE_UNKNOWN, OSVERSIONINFOEXW},
        SystemServices::{VER_NT_DOMAIN_CONTROLLER, VER_NT_SERVER, VER_NT_WORKSTATION},
        Threading::{GetCurrentProcess, IsWow64Process2},
    },
};

pub(crate) fn observe() -> HostFacts {
    let mut version = OSVERSIONINFOEXW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOEXW>() as u32,
        ..Default::default()
    };
    // SAFETY: RtlGetVersion accepts this initialized EX descriptor by its size and SDK prefix ABI.
    // Unlike GetVersionEx/Version Helpers, this read does not depend on an application manifest.
    let status = unsafe { RtlGetVersion((&mut version as *mut OSVERSIONINFOEXW).cast()) };
    let version = if status == 0
        && version.dwMajorVersion != 0
        && version.dwBuildNumber != 0
        && matches!(
            u32::from(version.wProductType),
            VER_NT_WORKSTATION | VER_NT_SERVER | VER_NT_DOMAIN_CONTROLLER
        ) {
        Fact::Known(WindowsVersion {
            major: version.dwMajorVersion,
            build: version.dwBuildNumber,
            workstation: u32::from(version.wProductType) == VER_NT_WORKSTATION,
        })
    } else {
        Fact::Unknown
    };
    let mut process_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    // SAFETY: current-process pseudo-handle is queryable; both initialized outputs remain local.
    // pNativeMachine identifies the host even when this x64 process is emulated on ARM64.
    let observed = unsafe {
        IsWow64Process2(
            GetCurrentProcess(),
            &mut process_machine,
            &mut native_machine,
        )
    };
    HostFacts {
        version,
        native_machine: if observed != 0 && native_machine != IMAGE_FILE_MACHINE_UNKNOWN {
            Fact::Known(native_machine)
        } else {
            Fact::Unknown
        },
    }
}
