//! Driver package and adapter node mutations for the elevated helper (WP-W4.1c, T18). Each function
//! performs one requested step through SetupAPI, NewDev or CfgMgr and reports a failure as
//! `HelperError`. The dispatcher (T19) decides when they run and undoes what this run created.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use crosspane_installer_core::elevated::{HARDWARE_ID, is_published_name};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DICD_GENERATE_ID, DIF_REGISTERDEVICE, DIF_REMOVE, DiUninstallDevice, DiUninstallDriverW,
    GUID_DEVCLASS_DISPLAY, HDEVINFO, INSTALLFLAG_NONINTERACTIVE, MAX_CLASS_NAME_LEN,
    MAX_DEVICE_ID_LEN, SP_COPY_NOOVERWRITE, SP_DEVINFO_DATA, SPDRP_HARDWAREID, SPOST_PATH,
    SetupCopyOEMInfW, SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList,
    SetupDiCreateDeviceInfoW, SetupDiDestroyDeviceInfoList, SetupDiGetDeviceInstanceIdW,
    SetupDiGetINFClassW, SetupDiOpenDeviceInfoW, SetupDiSetDeviceRegistryPropertyW,
    SetupGetNonInteractiveMode, SetupSetNonInteractiveMode, UpdateDriverForPlugAndPlayDevicesW,
};
use windows_sys::Win32::Foundation::{
    ERROR_FILE_EXISTS, FALSE, GetLastError, INVALID_HANDLE_VALUE, MAX_PATH, TRUE,
};
use windows_sys::core::{BOOL, GUID};

use super::{HelperError, HelperResult};

/// Longest native argument accepted, in UTF-16 units (the extended-length path limit).
const MAX_ARGUMENT_UNITS: usize = 32_767;

/// A package copied into the driver store by `stage`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Staged {
    pub(crate) published: String,
    pub(crate) newly: bool,
}

/// Switches SetupAPI into non-interactive mode for this process, so no UI can appear.
pub(crate) fn noninteractive() -> HelperResult<()> {
    // The setter returns the *previous* mode, not success, so its result is not an error signal.
    // SAFETY: takes a plain flag and changes only this process's SetupAPI interaction mode.
    let _previous = unsafe { SetupSetNonInteractiveMode(TRUE) };
    // SAFETY: reads this process's SetupAPI interaction mode; no arguments, no state change.
    if unsafe { SetupGetNonInteractiveMode() } == FALSE {
        return Err(HelperError::Unavailable("SetupAPI non-interactive mode"));
    }
    Ok(())
}

/// Copies `source_inf` into the driver store without overwriting. `newly` is false when the same
/// package was already staged; `published` is its `oemNN.inf` leaf name either way.
pub(crate) fn stage(source_inf: &Path) -> HelperResult<Staged> {
    let source = terminated(source_inf.as_os_str().encode_wide().collect())?;
    let mut destination = [0u16; MAX_PATH as usize];
    // SAFETY: `source` is NUL-terminated and lives for the call; SPOST_PATH makes it a full path with
    // no media location; `destination` is writable and its length is given in characters; the
    // optional component and required-size outputs are null.
    let copied = unsafe {
        SetupCopyOEMInfW(
            source.as_ptr(),
            null(),
            SPOST_PATH,
            SP_COPY_NOOVERWRITE,
            destination.as_mut_ptr(),
            MAX_PATH,
            null_mut(),
            null_mut(),
        )
    };
    let newly = if copied != FALSE {
        true
    } else {
        // SAFETY: reads the calling thread's last error code straight after the failed call.
        match unsafe { GetLastError() } {
            ERROR_FILE_EXISTS => false,
            code => return Err(HelperError::Win32("SetupCopyOEMInfW", code)),
        }
    };
    let missing = if newly {
        "published name"
    } else {
        "existing published name"
    };
    let text = text_of(&destination).ok_or(HelperError::Unavailable(missing))?;
    let leaf = text.rsplit('\\').next().unwrap_or(text.as_str());
    if !is_published_name(leaf) {
        return Err(HelperError::Unavailable(missing));
    }
    Ok(Staged {
        published: leaf.to_owned(),
        newly,
    })
}

/// Creates a root-enumerated display adapter node with Crosspane's hardware ID for the package
/// `source_inf`, and returns its instance ID. No driver is bound here; `bind` does that next.
pub(crate) fn create_device(source_inf: &Path) -> HelperResult<String> {
    let inf = terminated(source_inf.as_os_str().encode_wide().collect())?;
    let mut class = GUID::default();
    let mut class_name = [0u16; MAX_CLASS_NAME_LEN as usize];
    // SAFETY: `inf` is NUL-terminated and lives for the call; `class` is writable; `class_name` is
    // writable and its length is given in characters; the optional required-size output is null.
    if unsafe {
        SetupDiGetINFClassW(
            inf.as_ptr(),
            &mut class,
            class_name.as_mut_ptr(),
            MAX_CLASS_NAME_LEN,
            null_mut(),
        )
    } == FALSE
    {
        return Err(win32("SetupDiGetINFClassW"));
    }
    if !same_guid(&class, &GUID_DEVCLASS_DISPLAY) {
        return Err(HelperError::Refused(
            "the driver package is not a display class",
        ));
    }
    let class_units = before_nul(&class_name);
    if class_units.is_empty() {
        return Err(HelperError::Unavailable("driver class name"));
    }
    let class_wide = terminated(class_units.to_vec())?;
    let set = DeviceSet::new(Some(&class))?;
    let mut data = device_data();
    // SAFETY: `class_wide` is NUL-terminated and lives for the call; `class` is live; `data` is
    // writable with its size set; `set` is a live set owned by this function; the description and
    // parent window are null.
    if unsafe {
        SetupDiCreateDeviceInfoW(
            set.0,
            class_wide.as_ptr(),
            &class,
            null(),
            null_mut(),
            DICD_GENERATE_ID,
            &mut data,
        )
    } == FALSE
    {
        return Err(win32("SetupDiCreateDeviceInfoW"));
    }
    // REG_MULTI_SZ: the hardware ID, then the list terminator (UTF-16LE, two NULs).
    let hardware: Vec<u8> = HARDWARE_ID
        .encode_utf16()
        .chain([0, 0])
        .flat_map(u16::to_le_bytes)
        .collect();
    // SAFETY: `hardware` holds the REG_MULTI_SZ bytes and lives for the call; its size is in bytes;
    // `data` identifies the element created above in `set`. Nothing is registered yet, so a failure
    // here needs no undo: destroying the set discards the element.
    if unsafe {
        SetupDiSetDeviceRegistryPropertyW(
            set.0,
            &mut data,
            SPDRP_HARDWAREID,
            hardware.as_ptr(),
            hardware.len() as u32,
        )
    } == FALSE
    {
        return Err(win32("SetupDiSetDeviceRegistryPropertyW"));
    }
    // SAFETY: `set` and `data` describe the element created above; DIF_REGISTERDEVICE only registers it.
    if unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, &data) } == FALSE {
        return Err(win32("SetupDiCallClassInstaller"));
    }
    match instance_id(set.0, &data) {
        Ok(id) => Ok(id),
        Err(error) => {
            // SAFETY: undoes the node registered above while `set` and `data` are still live. The
            // result of the removal cannot change the error reported, so it is not inspected.
            unsafe { SetupDiCallClassInstaller(DIF_REMOVE, set.0, &data) };
            Err(error)
        }
    }
}

/// Binds the published package `published_inf` to every device with Crosspane's hardware ID.
/// Returns true when a reboot is needed before the driver is active.
pub(crate) fn bind(published_inf: &Path) -> HelperResult<bool> {
    let hardware = terminated(HARDWARE_ID.encode_utf16().collect())?;
    let inf = terminated(published_inf.as_os_str().encode_wide().collect())?;
    let mut reboot: BOOL = FALSE;
    // SAFETY: both strings are NUL-terminated and live for the call; `reboot` is writable; the parent
    // window is null, so nothing can be shown.
    if unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            null_mut(),
            hardware.as_ptr(),
            inf.as_ptr(),
            INSTALLFLAG_NONINTERACTIVE,
            &mut reboot,
        )
    } == FALSE
    {
        return Err(win32("UpdateDriverForPlugAndPlayDevicesW"));
    }
    Ok(reboot != FALSE)
}

/// Removes the adapter node `instance_id` and its device. Returns true when a reboot is needed.
pub(crate) fn remove_device(instance_id: &str) -> HelperResult<bool> {
    let id = terminated(instance_id.encode_utf16().collect())?;
    if id.len() > MAX_DEVICE_ID_LEN as usize {
        return Err(HelperError::Refused("the device instance ID is too long"));
    }
    let set = DeviceSet::new(None)?;
    let mut data = device_data();
    // SAFETY: `id` is NUL-terminated and lives for the call; `set` is live; `data` is writable with its
    // size set; the parent window is null and the open flags are zero.
    if unsafe { SetupDiOpenDeviceInfoW(set.0, id.as_ptr(), null_mut(), 0, &mut data) } == FALSE {
        return Err(win32("SetupDiOpenDeviceInfoW"));
    }
    let mut reboot: BOOL = FALSE;
    // SAFETY: `set` and `data` describe the device just opened; the parent window is null and the
    // flags are zero; `reboot` is writable.
    if unsafe { DiUninstallDevice(null_mut(), set.0, &data, 0, &mut reboot) } == FALSE {
        return Err(win32("DiUninstallDevice"));
    }
    Ok(reboot != FALSE)
}

/// Deletes the published package `published_inf` from the driver store. Returns true when a reboot is
/// needed. Only `oemNN.inf` leaf names are accepted, so a foreign file is never deleted.
pub(crate) fn delete_package(published_inf: &Path) -> HelperResult<bool> {
    let leaf = published_inf
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !is_published_name(leaf) {
        return Err(HelperError::Refused("not a published package name"));
    }
    let inf = terminated(published_inf.as_os_str().encode_wide().collect())?;
    let mut reboot: BOOL = FALSE;
    // SAFETY: `inf` is NUL-terminated and lives for the call; `reboot` is writable; the parent window
    // is null and the flags are zero.
    if unsafe { DiUninstallDriverW(null_mut(), inf.as_ptr(), 0, &mut reboot) } == FALSE {
        return Err(win32("DiUninstallDriverW"));
    }
    Ok(reboot != FALSE)
}

/// A SetupAPI device information set, destroyed on drop so every path releases it.
struct DeviceSet(HDEVINFO);

impl DeviceSet {
    /// An empty set for `class`, or a set that can hold devices of any class when `class` is `None`.
    fn new(class: Option<&GUID>) -> HelperResult<Self> {
        let class = class.map_or(null(), std::ptr::from_ref);
        // SAFETY: `class` is null or points at a GUID that lives for the call; the parent window is null.
        let set = unsafe { SetupDiCreateDeviceInfoList(class, null_mut()) };
        if set == INVALID_HANDLE_VALUE as HDEVINFO {
            return Err(win32("SetupDiCreateDeviceInfoList"));
        }
        Ok(Self(set))
    }
}

impl Drop for DeviceSet {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from SetupDiCreateDeviceInfoList and is destroyed only here.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// An empty SetupAPI device element with its size field set, as SetupAPI requires.
fn device_data() -> SP_DEVINFO_DATA {
    SP_DEVINFO_DATA {
        cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    }
}

/// Reads the instance ID of the element `data` in `set`.
fn instance_id(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> HelperResult<String> {
    let mut buffer = [0u16; MAX_DEVICE_ID_LEN as usize];
    // SAFETY: `buffer` is writable and its length is given in characters; the optional required-size
    // output is null; `set` and `data` describe a live element.
    if unsafe {
        SetupDiGetDeviceInstanceIdW(
            set,
            data,
            buffer.as_mut_ptr(),
            MAX_DEVICE_ID_LEN,
            null_mut(),
        )
    } == FALSE
    {
        return Err(win32("SetupDiGetDeviceInstanceIdW"));
    }
    text_of(&buffer).ok_or(HelperError::Unavailable("device instance ID"))
}

/// The last error of the call that just failed, as a `HelperError`.
fn win32(call: &'static str) -> HelperError {
    // SAFETY: reads the calling thread's last error code; no state is changed.
    HelperError::Win32(call, unsafe { GetLastError() })
}

/// Appends the NUL terminator, refusing empty, oversized or embedded-NUL arguments before any effect.
fn terminated(mut units: Vec<u16>) -> HelperResult<Vec<u16>> {
    if units.is_empty() || units.len() >= MAX_ARGUMENT_UNITS || units.contains(&0) {
        return Err(HelperError::Refused("invalid native argument"));
    }
    units.push(0);
    Ok(units)
}

/// The units before the first NUL, or all of them when there is none.
fn before_nul(units: &[u16]) -> &[u16] {
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    &units[..end]
}

/// The non-empty text before the first NUL, if it is valid UTF-16.
fn text_of(units: &[u16]) -> Option<String> {
    let text = String::from_utf16(before_nul(units)).ok()?;
    (!text.is_empty()).then_some(text)
}

/// Field-wise GUID equality (windows-sys' `GUID` does not implement `PartialEq`).
fn same_guid(left: &GUID, right: &GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}
