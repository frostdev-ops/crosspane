//! Control-device discovery (WP-W3.1b): finds Crosspane's one IddCx control interface, checks the
//! hardware ID of its device, and names the single display adapter on that device. Configuration
//! manager queries only; no interface is opened here.
#![allow(unsafe_code)]

use crate::model::{
    cpd,
    twin::{TwinError, hardware_id_matches, parse_multi_sz, select_control_interface},
};
use std::{iter::once, ptr::null};
use windows_sys::{
    Win32::Devices::{
        DeviceAndDriverInstallation::{
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_DevNode_PropertyW,
            CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW,
            CM_Get_Device_Interface_PropertyW, CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW,
            CONFIGRET, CR_NO_SUCH_DEVNODE, CR_SUCCESS,
        },
        Display::GUID_DEVINTERFACE_DISPLAY_ADAPTER,
        Properties::{
            DEVPKEY_Device_HardwareIds, DEVPKEY_Device_InstanceId, DEVPROP_TYPE_STRING,
            DEVPROP_TYPE_STRING_LIST, DEVPROPTYPE,
        },
    },
    core::GUID,
};

/// Bound on one interface list or property read, in UTF-16 units (the p9f spike's limit).
const LIST_MAX_WORDS: usize = 8_192;
/// Bound on one device path or instance ID, in UTF-16 units.
const PATH_MAX_UNITS: usize = 512;
/// Byte buffer for one property read: the same bound in bytes.
const PROPERTY_MAX_BYTES: usize = 2 * LIST_MAX_WORDS;

/// Crosspane's control device. Every path here is our own, found under the control interface.
#[derive(Debug)]
#[allow(dead_code)] // used by twin/mod.rs (T9)
pub(crate) struct ControlDevice {
    pub(crate) interface: String,
    pub(crate) instance: String,
    pub(crate) adapter: String,
}

/// Finds the one control interface, checks that its device carries Crosspane's hardware ID, and
/// names the single display adapter of that device. `Absent` means the driver is not installed.
/// `Foreign` means another device exposes our interface GUID.
pub(crate) fn locate() -> Result<ControlDevice, TwinError> {
    let control = GUID::from_u128(cpd::CONTROL_INTERFACE_GUID);
    let interfaces = interface_list(&control, None)?;
    let interface = select_control_interface(&interfaces)?.to_owned();
    let instance = interface_instance(&interface)?;
    if !hardware_id_matches(&hardware_ids(&instance)?) {
        return Err(TwinError::Foreign);
    }
    let adapters = interface_list(&GUID_DEVINTERFACE_DISPLAY_ADAPTER, Some(&instance))?;
    let adapter = select_control_interface(&adapters)?.to_owned();
    Ok(ControlDevice {
        interface,
        instance,
        adapter,
    })
}

/// PRESENT device interfaces of `class`, optionally only those of one device instance. An empty
/// list is a single NUL here, or the double NUL of a MULTI_SZ; both mean no entries.
fn interface_list(class: &GUID, device: Option<&str>) -> Result<Vec<String>, TwinError> {
    let filter = device.map(wide).transpose()?;
    let filter_ptr = filter.as_ref().map_or(null(), |id| id.as_ptr());
    let mut length: u32 = 0;
    // SAFETY: `class` is a live GUID borrow; `filter_ptr` is null or the NUL-terminated `filter`
    // buffer, which outlives the call; `length` is a valid out pointer; PRESENT is the only flag.
    let status = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &mut length,
            class,
            filter_ptr,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if status != CR_SUCCESS {
        return Err(TwinError::Native("interface list size", status));
    }
    if length == 0 || length as usize > LIST_MAX_WORDS {
        return Err(TwinError::Protocol("interface list size is out of bounds"));
    }
    let mut list = vec![0u16; length as usize];
    // SAFETY: the same class and filter; `list` is writable for exactly `length` UTF-16 units, the
    // count the size query reported, and the length passed is that same count.
    let status = unsafe {
        CM_Get_Device_Interface_ListW(
            class,
            filter_ptr,
            list.as_mut_ptr(),
            length,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if status != CR_SUCCESS {
        return Err(TwinError::Native("interface list", status));
    }
    if list == [0] {
        return Ok(Vec::new());
    }
    parse_multi_sz(&list)
}

/// The device instance ID that owns one device interface.
fn interface_instance(interface: &str) -> Result<String, TwinError> {
    let path = wide(interface)?;
    let mut kind: DEVPROPTYPE = 0;
    let mut bytes = PROPERTY_MAX_BYTES as u32;
    let mut data = vec![0u8; PROPERTY_MAX_BYTES];
    // SAFETY: `path` is a NUL-terminated interface path that outlives the call; the key is a
    // static; `kind` and `bytes` are valid out pointers; `data` is writable for `bytes` bytes and
    // the call never writes more than the size it is given. Reserved flags are zero.
    let status = unsafe {
        CM_Get_Device_Interface_PropertyW(
            path.as_ptr(),
            &DEVPKEY_Device_InstanceId,
            &mut kind,
            data.as_mut_ptr(),
            &mut bytes,
            0,
        )
    };
    let data = checked_property(
        "interface instance",
        status,
        kind,
        DEVPROP_TYPE_STRING,
        bytes,
        data,
    )?;
    single_string(&data)
}

/// The hardware IDs of a device instance. An instance with no devnode is `Absent`.
fn hardware_ids(instance: &str) -> Result<Vec<String>, TwinError> {
    let id = wide(instance)?;
    let mut devnode: u32 = 0;
    // SAFETY: `id` is a NUL-terminated instance ID that outlives the call; `devnode` is a valid
    // out pointer; the flags are NORMAL.
    let status = unsafe { CM_Locate_DevNodeW(&mut devnode, id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) };
    if status == CR_NO_SUCH_DEVNODE {
        return Err(TwinError::Absent);
    }
    if status != CR_SUCCESS {
        return Err(TwinError::Native("device node", status));
    }
    let mut kind: DEVPROPTYPE = 0;
    let mut bytes = PROPERTY_MAX_BYTES as u32;
    let mut data = vec![0u8; PROPERTY_MAX_BYTES];
    // SAFETY: `devnode` is the handle just located; the key is a static; `kind` and `bytes` are
    // valid out pointers; `data` is writable for `bytes` bytes. Reserved flags are zero.
    let status = unsafe {
        CM_Get_DevNode_PropertyW(
            devnode,
            &DEVPKEY_Device_HardwareIds,
            &mut kind,
            data.as_mut_ptr(),
            &mut bytes,
            0,
        )
    };
    let data = checked_property(
        "hardware IDs",
        status,
        kind,
        DEVPROP_TYPE_STRING_LIST,
        bytes,
        data,
    )?;
    parse_multi_sz(&utf16_words(&data)?)
}

/// Checks a property read's status, type and size, then keeps exactly the bytes it reported.
fn checked_property(
    what: &'static str,
    status: CONFIGRET,
    kind: DEVPROPTYPE,
    expected: DEVPROPTYPE,
    bytes: u32,
    mut data: Vec<u8>,
) -> Result<Vec<u8>, TwinError> {
    if status != CR_SUCCESS {
        return Err(TwinError::Native(what, status));
    }
    if kind != expected || bytes as usize > data.len() {
        return Err(TwinError::Protocol(
            "device property has an unexpected type or size",
        ));
    }
    data.truncate(bytes as usize);
    Ok(data)
}

/// NUL-terminated UTF-16 for a device path or instance ID, bounded to `PATH_MAX_UNITS`.
fn wide(value: &str) -> Result<Vec<u16>, TwinError> {
    let units = value.encode_utf16().count();
    if units == 0 || units > PATH_MAX_UNITS {
        return Err(TwinError::Protocol("device path is empty or out of bounds"));
    }
    Ok(value.encode_utf16().chain(once(0)).collect())
}

/// Little-endian UTF-16 units of a property read.
fn utf16_words(data: &[u8]) -> Result<Vec<u16>, TwinError> {
    if !data.len().is_multiple_of(2) {
        return Err(TwinError::Protocol("device property is not UTF-16"));
    }
    Ok(data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|unit| u16::from_le_bytes(*unit))
        .collect())
}

/// One nonempty NUL-terminated string, bounded, without interior NULs or control characters.
fn single_string(data: &[u8]) -> Result<String, TwinError> {
    let words = utf16_words(data)?;
    let (terminator, text) = words
        .split_last()
        .ok_or(TwinError::Protocol("device string is empty"))?;
    if *terminator != 0 || text.is_empty() || text.len() > PATH_MAX_UNITS || text.contains(&0) {
        return Err(TwinError::Protocol(
            "device string is not one bounded string",
        ));
    }
    let value =
        String::from_utf16(text).map_err(|_| TwinError::Protocol("device string is not UTF-16"))?;
    if value.chars().any(char::is_control) {
        return Err(TwinError::Protocol("device string has a control character"));
    }
    Ok(value)
}
