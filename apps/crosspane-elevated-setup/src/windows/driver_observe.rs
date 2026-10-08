//! Read-only driver observation for the elevated helper (WP-W4.1c, T17). Lists Crosspane's packages
//! in the driver store, reads the source package next to the helper, and finds the display-class
//! devices that list Crosspane's hardware ID. Nothing here changes the system. Foreign packages and
//! devices are compared and dropped; their names are never stored or printed.

use super::{HelperError, HelperResult};
use crosspane_installer_core::elevated::driver::{
    DRIVER_BINARY, DRIVER_CATALOG, DRIVER_INF, DeviceFacts, MAX_DEVICES, MAX_PACKAGES, PackageFacts,
};
use crosspane_installer_core::elevated::{HARDWARE_ID, is_published_name};
use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, CR_NO_SUCH_DEVINST, CR_SUCCESS, DN_HAS_PROBLEM, GUID_DEVCLASS_DISPLAY,
    HDEVINFO, INF_STYLE_WIN4, INFCONTEXT, INFINFO_INF_NAME_IS_ABSOLUTE, SP_DEVINFO_DATA,
    SP_ORIGINAL_FILE_INFO_W, SPDRP_HARDWAREID, SetupCloseInfFile, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW,
    SetupDiGetDevicePropertyW, SetupDiGetDeviceRegistryPropertyW, SetupFindFirstLineW,
    SetupFindNextLine, SetupGetFieldCount, SetupGetInfInformationW, SetupGetLineTextW,
    SetupGetStringFieldW, SetupOpenInfFileW, SetupQueryInfOriginalFileInformationW,
};
use windows_sys::Win32::Devices::Properties::{
    DEVPKEY_Device_DriverInfPath, DEVPROP_TYPE_STRING, DEVPROPTYPE,
};
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA, ERROR_LINE_NOT_FOUND,
    ERROR_NO_MORE_FILES, ERROR_NO_MORE_ITEMS, ERROR_NOT_FOUND, ERROR_SECTION_NOT_FOUND,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, FileAttributeTagInfo,
    FindClose, FindFirstFileW, FindNextFileW, GetFileInformationByHandleEx, GetFileType,
    OPEN_EXISTING, WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

/// Most `oem*.inf` names the store scan reads.
pub(crate) const MAX_INF_FILES: usize = 4096;
/// Units for `<windir>` and for one-value buffers (INF values, fields, instance IDs, driver paths).
const MAX_PATH_UNITS: usize = 1024;
const MAX_TEXT_UNITS: usize = 512;
/// `SPDRP_HARDWAREID` multi-sz, in units.
const MAX_MULTI_SZ_UNITS: usize = 4096;
/// `SetupGetInfInformationW` buffer, in bytes.
const MAX_INF_INFO_BYTES: usize = 64 * 1024;
/// INF lines read per package: the `[Manufacturer]` lines plus every model line.
const MAX_INF_LINES: usize = 512;
const MAX_DECORATIONS: u32 = 16;
const MAX_MODEL_SECTIONS: usize = 64;
const MAX_HARDWARE_IDS: usize = 64;
const MAX_DEVICE_ENUMERATION: usize = 4096;
const INVALID_DEVINFO: HDEVINFO = -1;
/// `REG_MULTI_SZ` (winnt.h). `Win32_System_Registry` is not enabled for this app.
const REG_MULTI_SZ: u32 = 7;

/// Crosspane's driver store packages: `oem*.inf` names whose original INF name is
/// `CrosspaneIdd.inf`. Every other package is skipped after its original name is compared.
pub(crate) fn packages() -> HelperResult<Vec<PackageFacts>> {
    let directory = inf_directory()?;
    let pattern = wide(directory.join("oem*.inf"));
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: pattern is NUL-terminated; data is a live output structure of the exact type.
    let raw = unsafe { FindFirstFileW(pattern.as_ptr(), &mut data) };
    if raw == INVALID_HANDLE_VALUE {
        return match last_error() {
            ERROR_FILE_NOT_FOUND | ERROR_NO_MORE_FILES => Ok(Vec::new()),
            code => Err(HelperError::Win32("FindFirstFileW", code)),
        };
    }
    let search = FindHandle(raw);
    let mut names = Vec::new();
    let mut entries = 0usize;
    loop {
        entries += 1;
        if entries > MAX_INF_FILES {
            return Err(HelperError::Oversize);
        }
        let name = lossy_text(&data.cFileName);
        let directory_entry = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        if !directory_entry && is_published_name(&name) {
            names.push(name);
        }
        // SAFETY: search is live; data is the output structure FindNextFileW fills.
        if unsafe { FindNextFileW(search.0, &mut data) } == 0 {
            let code = last_error();
            if code != ERROR_NO_MORE_FILES {
                return Err(HelperError::Win32("FindNextFileW", code));
            }
            break;
        }
    }
    let mut packages = Vec::new();
    for name in names {
        let path = directory.join(&name);
        let information = inf_information(&path)?;
        let original = original_file(&information)?;
        if !lossy_text(&original.OriginalInfName).eq_ignore_ascii_case(DRIVER_INF) {
            continue;
        }
        if packages.len() == MAX_PACKAGES {
            return Err(HelperError::Oversize);
        }
        let inf = read_inf(&path)?;
        // The store copy's CatalogFile line can name the renamed catalog, so the catalog is the
        // one SetupAPI recorded when the package was staged.
        packages.push(PackageFacts {
            published: name,
            original_inf: units_text(&original.OriginalInfName)?,
            original_catalog: units_text(&original.OriginalCatalogName)?,
            provider: inf.provider,
            class_guid: inf.class_guid,
            driver_version: inf.driver_version,
            hardware_ids: inf.hardware_ids,
        });
    }
    Ok(packages)
}

/// `<windir>\INF`, the driver store's root.
pub(crate) fn inf_directory() -> HelperResult<PathBuf> {
    let mut buffer = [0u16; MAX_PATH_UNITS];
    // SAFETY: buffer is live and its unit count is passed exactly.
    let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 {
        return Err(HelperError::Win32("GetWindowsDirectoryW", last_error()));
    }
    if length >= buffer.len() {
        return Err(HelperError::Oversize);
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..length])).join("INF"))
}

/// The Crosspane package next to the helper. Its three files and the directory are plain (no
/// reparse point), and `published` is empty.
pub(crate) fn source_package(directory: &Path) -> HelperResult<PackageFacts> {
    plain_object(directory, true)?;
    for name in [DRIVER_INF, DRIVER_CATALOG, DRIVER_BINARY] {
        plain_object(&directory.join(name), false)?;
    }
    let inf = read_inf(&directory.join(DRIVER_INF))?;
    Ok(PackageFacts {
        published: String::new(),
        original_inf: DRIVER_INF.to_owned(),
        original_catalog: inf.catalog,
        provider: inf.provider,
        class_guid: inf.class_guid,
        driver_version: inf.driver_version,
        hardware_ids: inf.hardware_ids,
    })
}

/// Display-class devices that list Crosspane's hardware ID, at most `MAX_DEVICES`. A device that
/// does not list it is never read further.
pub(crate) fn devices() -> HelperResult<Vec<DeviceFacts>> {
    // SAFETY: the display class GUID is a static value; no enumerator and no parent window are
    // given, and the flags are zero so that absent (ghost) devices are included.
    let raw = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVCLASS_DISPLAY,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        )
    };
    if raw == INVALID_DEVINFO {
        return Err(HelperError::Win32("SetupDiGetClassDevsW", last_error()));
    }
    let set = DeviceSet(raw);
    let mut devices = Vec::new();
    let mut index = 0u32;
    loop {
        let mut info = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: set is live; info is a live output structure whose size is in cbSize.
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut info) } == 0 {
            let code = last_error();
            if code == ERROR_NO_MORE_ITEMS {
                break;
            }
            return Err(HelperError::Win32("SetupDiEnumDeviceInfo", code));
        }
        if index as usize >= MAX_DEVICE_ENUMERATION {
            return Err(HelperError::Oversize);
        }
        if let Some(device) = observe_device(set.0, &info)? {
            if devices.len() == MAX_DEVICES {
                return Err(HelperError::Oversize);
            }
            devices.push(device);
        }
        index += 1;
    }
    Ok(devices)
}

/// Refuses anything that is not a plain object of the requested kind: a reparse point is never
/// followed, and a file must be a disk file. The handle is attributes-only and closed on return.
fn plain_object(path: &Path, directory: bool) -> HelperResult<()> {
    let name = wide(path);
    // SAFETY: name is NUL-terminated; the open is attributes-only, shares everything, and opens a
    // final reparse point rather than following it.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(HelperError::Win32("CreateFileW", last_error()));
    }
    // SAFETY: a successful CreateFileW transfers one owned handle, closed exactly once here.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: handle is live; tag is the structure FileAttributeTagInfo fills, with its exact size.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileAttributeTagInfo,
            (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(HelperError::Win32(
            "GetFileInformationByHandleEx",
            last_error(),
        ));
    }
    // SAFETY: handle is live and only queried for its type.
    let kind = unsafe { GetFileType(handle.as_raw_handle()) };
    let is_directory = tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let plain = tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
        && is_directory == directory
        && (directory || kind == FILE_TYPE_DISK);
    if plain {
        Ok(())
    } else {
        Err(HelperError::Refused(
            "a driver package object is not a plain file or directory",
        ))
    }
}

/// The `SetupGetInfInformationW` buffer for one absolute INF path, sized by a probe call. Its
/// length is in `u64`s so that the buffer is 8-byte aligned for `SP_INF_INFORMATION`.
fn inf_information(path: &Path) -> HelperResult<Vec<u64>> {
    let name = wide(path);
    let mut required = 0u32;
    // SAFETY: name is NUL-terminated; a null buffer of size zero only reports the required size.
    let probed = unsafe {
        SetupGetInfInformationW(
            name.as_ptr().cast(),
            INFINFO_INF_NAME_IS_ABSOLUTE,
            std::ptr::null_mut(),
            0,
            &mut required,
        )
    };
    // With a null buffer the call succeeds and only reports the size; some builds instead fail
    // with ERROR_INSUFFICIENT_BUFFER. Both carry the required size.
    if probed == 0 {
        let code = last_error();
        if code != ERROR_INSUFFICIENT_BUFFER {
            return Err(HelperError::Win32("SetupGetInfInformationW", code));
        }
    }
    let size = required as usize;
    if size == 0 {
        return Err(HelperError::Unavailable("SetupGetInfInformationW size"));
    }
    if size > MAX_INF_INFO_BYTES {
        return Err(HelperError::Oversize);
    }
    let mut buffer = vec![0u64; size.div_ceil(8)];
    let mut written = 0u32;
    // SAFETY: buffer is 8-byte aligned and holds `size` bytes, and that exact size is passed; name
    // is NUL-terminated; written is a live output.
    let ok = unsafe {
        SetupGetInfInformationW(
            name.as_ptr().cast(),
            INFINFO_INF_NAME_IS_ABSOLUTE,
            buffer.as_mut_ptr().cast(),
            size as u32,
            &mut written,
        )
    };
    if ok == 0 {
        return Err(HelperError::Win32("SetupGetInfInformationW", last_error()));
    }
    Ok(buffer)
}

/// The original INF and catalog names of the INF in `information` (index 0, its only INF).
fn original_file(information: &[u64]) -> HelperResult<SP_ORIGINAL_FILE_INFO_W> {
    let mut original = SP_ORIGINAL_FILE_INFO_W {
        cbSize: std::mem::size_of::<SP_ORIGINAL_FILE_INFO_W>() as u32,
        ..Default::default()
    };
    // SAFETY: information is the buffer from SetupGetInfInformationW for this INF; index 0 is its
    // only INF; no alternate platform is given; original is a live output with cbSize set.
    let ok = unsafe {
        SetupQueryInfOriginalFileInformationW(
            information.as_ptr().cast(),
            0,
            std::ptr::null(),
            &mut original,
        )
    };
    if ok == 0 {
        return Err(HelperError::Win32(
            "SetupQueryInfOriginalFileInformationW",
            last_error(),
        ));
    }
    Ok(original)
}

/// The facts of one INF, read with SetupAPI. A missing `[Version]` value is empty, which the
/// OS-free comparison refuses.
struct InfFacts {
    provider: String,
    class_guid: String,
    catalog: String,
    driver_version: String,
    hardware_ids: Vec<String>,
}

fn read_inf(path: &Path) -> HelperResult<InfFacts> {
    let name = wide(path);
    let mut error_line = 0u32;
    // SAFETY: name is NUL-terminated; a null class name opens the INF under its own class; the
    // error line is a live output.
    let raw = unsafe {
        SetupOpenInfFileW(
            name.as_ptr(),
            std::ptr::null(),
            INF_STYLE_WIN4,
            &mut error_line,
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(HelperError::Win32("SetupOpenInfFileW", last_error()));
    }
    let inf = InfFile(raw);
    Ok(InfFacts {
        provider: version_value(&inf, "Provider")?,
        class_guid: version_value(&inf, "ClassGuid")?,
        catalog: version_value(&inf, "CatalogFile")?,
        driver_version: version_value(&inf, "DriverVer")?,
        hardware_ids: inf.hardware_ids()?,
    })
}

/// One `[Version]` value. A `%token%` is resolved through `[Strings]` and one pair of surrounding
/// quotes is removed, so the comparison sees the INF's text whichever way SetupAPI returned it.
fn version_value(inf: &InfFile, key: &str) -> HelperResult<String> {
    let raw = inf.line_text("Version", key)?.unwrap_or_default();
    let token = raw
        .strip_prefix('%')
        .and_then(|rest| rest.strip_suffix('%'))
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    let value = match token {
        Some(token) => inf.line_text("Strings", &token)?.unwrap_or_default(),
        None => raw,
    };
    let unquoted = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value.as_str());
    Ok(unquoted.to_owned())
}

/// An open INF handle, closed on every return path.
struct InfFile(*mut c_void);

impl InfFile {
    /// The text of `key` in `[section]`, or `None` when the section or key is absent.
    fn line_text(&self, section: &str, key: &str) -> HelperResult<Option<String>> {
        let section = wide(section);
        let key = wide(key);
        let mut buffer = [0u16; MAX_TEXT_UNITS];
        let mut required = 0u32;
        // SAFETY: self holds a live INF handle; section and key are NUL-terminated; buffer and
        // required are live, and the buffer's unit count is passed.
        let found = unsafe {
            SetupGetLineTextW(
                std::ptr::null(),
                self.0,
                section.as_ptr(),
                key.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut required,
            )
        };
        if found == 0 {
            return match last_error() {
                ERROR_LINE_NOT_FOUND | ERROR_SECTION_NOT_FOUND => Ok(None),
                ERROR_INSUFFICIENT_BUFFER => Err(HelperError::Oversize),
                code => Err(HelperError::Win32("SetupGetLineTextW", code)),
            };
        }
        units_text(&buffer).map(Some)
    }

    /// The hardware IDs of every model line. `[Manufacturer]` names one models section per
    /// decoration (`models.decoration`, or `models` when there is none); field 2 of each line in
    /// those sections is a hardware ID.
    fn hardware_ids(&self) -> HelperResult<Vec<String>> {
        let mut budget = MAX_INF_LINES;
        let mut sections: Vec<String> = Vec::new();
        self.each_line("Manufacturer", &mut budget, |line| {
            let count = field_count(line);
            if count == 0 {
                return Ok(());
            }
            if count > MAX_DECORATIONS + 1 {
                return Err(HelperError::Oversize);
            }
            let models = field(line, 1)?;
            if count == 1 {
                push_section(&mut sections, models)?;
            } else {
                for index in 2..=count {
                    let decoration = field(line, index)?;
                    push_section(&mut sections, format!("{models}.{decoration}"))?;
                }
            }
            Ok(())
        })?;
        let mut ids = Vec::new();
        for section in &sections {
            self.each_line(section, &mut budget, |line| {
                if field_count(line) < 2 {
                    return Ok(());
                }
                let id = field(line, 2)?;
                if id.is_empty() {
                    return Ok(());
                }
                if ids.len() == MAX_HARDWARE_IDS {
                    return Err(HelperError::Oversize);
                }
                ids.push(id);
                Ok(())
            })?;
        }
        Ok(ids)
    }

    /// Visits each line of `[section]` in order, spending one unit of `budget` per line. A missing
    /// section has no lines.
    fn each_line(
        &self,
        section: &str,
        budget: &mut usize,
        mut visit: impl FnMut(&INFCONTEXT) -> HelperResult<()>,
    ) -> HelperResult<()> {
        let section = wide(section);
        let mut line = INFCONTEXT::default();
        // SAFETY: self holds a live INF handle; section is NUL-terminated; line is a live output.
        if unsafe { SetupFindFirstLineW(self.0, section.as_ptr(), std::ptr::null(), &mut line) }
            == 0
        {
            return match last_error() {
                ERROR_SECTION_NOT_FOUND | ERROR_LINE_NOT_FOUND => Ok(()),
                code => Err(HelperError::Win32("SetupFindFirstLineW", code)),
            };
        }
        loop {
            *budget = budget.checked_sub(1).ok_or(HelperError::Oversize)?;
            visit(&line)?;
            let mut next = INFCONTEXT::default();
            // SAFETY: line is a live context from SetupFindFirstLineW or the previous
            // SetupFindNextLine; next is a live output.
            if unsafe { SetupFindNextLine(&line, &mut next) } == 0 {
                return match last_error() {
                    ERROR_LINE_NOT_FOUND => Ok(()),
                    code => Err(HelperError::Win32("SetupFindNextLine", code)),
                };
            }
            line = next;
        }
    }
}

impl Drop for InfFile {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful SetupOpenInfFileW and is closed exactly once.
        unsafe { SetupCloseInfFile(self.0) };
    }
}

fn push_section(sections: &mut Vec<String>, name: String) -> HelperResult<()> {
    if sections.len() == MAX_MODEL_SECTIONS {
        return Err(HelperError::Oversize);
    }
    sections.push(name);
    Ok(())
}

/// Field `index` of a line: 0 is the key, 1 is the first value, and so on.
fn field(line: &INFCONTEXT, index: u32) -> HelperResult<String> {
    let mut units = [0u16; MAX_TEXT_UNITS];
    let mut required = 0u32;
    // SAFETY: line is a live context; units is live and its unit count is passed; required is a
    // live output.
    let ok = unsafe {
        SetupGetStringFieldW(
            line,
            index,
            units.as_mut_ptr(),
            units.len() as u32,
            &mut required,
        )
    };
    if ok == 0 {
        return Err(match last_error() {
            ERROR_INSUFFICIENT_BUFFER => HelperError::Oversize,
            code => HelperError::Win32("SetupGetStringFieldW", code),
        });
    }
    units_text(&units)
}

fn field_count(line: &INFCONTEXT) -> u32 {
    // SAFETY: line is a live context from this INF's enumeration.
    unsafe { SetupGetFieldCount(line) }
}

/// One enumerated device, or `None` when it does not list Crosspane's hardware ID.
fn observe_device(set: HDEVINFO, info: &SP_DEVINFO_DATA) -> HelperResult<Option<DeviceFacts>> {
    let hardware_ids = device_hardware_ids(set, info)?;
    if !hardware_ids
        .iter()
        .any(|id| id.eq_ignore_ascii_case(HARDWARE_ID))
    {
        return Ok(None);
    }
    let instance_id = device_instance_id(set, info)?;
    let driver_inf = device_driver_inf(set, info)?;
    let (present, problem) = device_status(info.DevInst)?;
    Ok(Some(DeviceFacts {
        instance_id,
        hardware_ids,
        driver_inf,
        problem,
        present,
    }))
}

fn device_hardware_ids(set: HDEVINFO, info: &SP_DEVINFO_DATA) -> HelperResult<Vec<String>> {
    let mut units = vec![0u16; MAX_MULTI_SZ_UNITS];
    let mut kind = 0u32;
    let mut required = 0u32;
    // SAFETY: set and info come from this enumeration; units is live and its byte size is passed;
    // kind and required are live outputs.
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set,
            info,
            SPDRP_HARDWAREID,
            &mut kind,
            units.as_mut_ptr().cast(),
            (units.len() * 2) as u32,
            &mut required,
        )
    };
    if ok == 0 {
        return match last_error() {
            // The property is absent: the device lists no hardware IDs.
            ERROR_INVALID_DATA => Ok(Vec::new()),
            ERROR_INSUFFICIENT_BUFFER => Err(HelperError::Oversize),
            code => Err(HelperError::Win32(
                "SetupDiGetDeviceRegistryPropertyW",
                code,
            )),
        };
    }
    if kind != REG_MULTI_SZ {
        return Err(HelperError::Unavailable("hardware id type"));
    }
    let used = (required as usize / 2).min(units.len());
    multi_sz(&units[..used])
}

/// The strings of a REG_MULTI_SZ value: each NUL-terminated, and the list ends with an empty one.
fn multi_sz(units: &[u16]) -> HelperResult<Vec<String>> {
    let mut values = Vec::new();
    for segment in units.split(|unit| *unit == 0) {
        if segment.is_empty() {
            break;
        }
        if values.len() == MAX_HARDWARE_IDS {
            return Err(HelperError::Oversize);
        }
        values.push(
            String::from_utf16(segment)
                .map_err(|_| HelperError::Unavailable("hardware id is not UTF-16"))?,
        );
    }
    Ok(values)
}

fn device_instance_id(set: HDEVINFO, info: &SP_DEVINFO_DATA) -> HelperResult<String> {
    let mut units = [0u16; MAX_TEXT_UNITS];
    let mut required = 0u32;
    // SAFETY: set and info come from this enumeration; units is live and its unit count is passed;
    // required is a live output.
    let ok = unsafe {
        SetupDiGetDeviceInstanceIdW(
            set,
            info,
            units.as_mut_ptr(),
            units.len() as u32,
            &mut required,
        )
    };
    if ok == 0 {
        return Err(match last_error() {
            ERROR_INSUFFICIENT_BUFFER => HelperError::Oversize,
            code => HelperError::Win32("SetupDiGetDeviceInstanceIdW", code),
        });
    }
    units_text(&units)
}

/// The published INF that drives the device, or `None` when no driver is bound.
fn device_driver_inf(set: HDEVINFO, info: &SP_DEVINFO_DATA) -> HelperResult<Option<String>> {
    let mut units = [0u16; MAX_TEXT_UNITS];
    let mut kind: DEVPROPTYPE = 0;
    let mut required = 0u32;
    // SAFETY: set and info come from this enumeration; the key is a static DEVPROPKEY; units is
    // live and its byte size is passed; kind and required are live outputs; flags are zero.
    let ok = unsafe {
        SetupDiGetDevicePropertyW(
            set,
            info,
            &DEVPKEY_Device_DriverInfPath,
            &mut kind,
            units.as_mut_ptr().cast(),
            (units.len() * 2) as u32,
            &mut required,
            0,
        )
    };
    if ok == 0 {
        return match last_error() {
            ERROR_NOT_FOUND => Ok(None),
            ERROR_INSUFFICIENT_BUFFER => Err(HelperError::Oversize),
            code => Err(HelperError::Win32("SetupDiGetDevicePropertyW", code)),
        };
    }
    if kind != DEVPROP_TYPE_STRING {
        return Err(HelperError::Unavailable("driver inf property type"));
    }
    let inf = units_text(&units)?;
    Ok((!inf.is_empty()).then_some(inf))
}

/// Presence and problem code. `CR_NO_SUCH_DEVINST` is a ghost node: absent, with no problem.
fn device_status(devinst: u32) -> HelperResult<(bool, Option<u32>)> {
    let mut status = 0u32;
    let mut problem = 0u32;
    // SAFETY: both outputs are live; devinst comes from this enumeration; flags are zero.
    let code = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    match code {
        CR_SUCCESS => Ok((
            true,
            Some(if status & DN_HAS_PROBLEM != 0 {
                problem
            } else {
                0
            }),
        )),
        CR_NO_SUCH_DEVINST => Ok((false, None)),
        code => Err(HelperError::Win32("CM_Get_DevNode_Status", code)),
    }
}

/// A SetupAPI device information set, destroyed on every return path.
struct DeviceSet(HDEVINFO);

impl Drop for DeviceSet {
    fn drop(&mut self) {
        // SAFETY: the set came from a successful SetupDiGetClassDevsW and is destroyed exactly once.
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// A FindFirstFileW search, closed on every return path.
struct FindHandle(HANDLE);

impl Drop for FindHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful FindFirstFileW and is closed exactly once.
        unsafe { FindClose(self.0) };
    }
}

/// The calling thread's last Win32 error (SetupAPI and CfgMgr report theirs the same way).
fn last_error() -> u32 {
    // SAFETY: GetLastError has no preconditions and only reads the thread's error value.
    unsafe { GetLastError() }
}

/// NUL-terminated UTF-16 for a Win32 string argument.
fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The units before the first NUL. A buffer without a NUL is malformed.
fn units_before_nul(units: &[u16]) -> HelperResult<&[u16]> {
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .ok_or(HelperError::Unavailable("text has no terminator"))?;
    Ok(&units[..end])
}

fn units_text(units: &[u16]) -> HelperResult<String> {
    String::from_utf16(units_before_nul(units)?)
        .map_err(|_| HelperError::Unavailable("text is not UTF-16"))
}

/// The units before the first NUL, decoded leniently. Used only to compare names: a name with a
/// lone surrogate cannot equal an ASCII name, so it compares unequal.
fn lossy_text(units: &[u16]) -> String {
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}
