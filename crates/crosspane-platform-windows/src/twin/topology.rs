//! Own-adapter topology for the twin client (WP-W3.1b). `own_paths` finds the active display paths
//! that belong to our IddCx adapter, and `monitor_dpi` reads a twin's DPI from a never-shown PMv2
//! helper window. Foreign adapters are read only for their adapter interface comparison: no
//! foreign name, monitor, source or geometry is queried, stored or printed.
#![allow(unsafe_code)]

use crate::model::twin::{OwnPath, TwinError, interface_path_eq};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    mem::size_of,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Devices::Display::*,
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

/// Bounds from `src/displays.rs`: 128 paths and three modes per path.
const PATH_LIMIT: usize = 128;
const MODE_LIMIT: usize = PATH_LIMIT * 3;
/// `ERROR_INSUFFICIENT_BUFFER` is retried this many times after the first attempt.
const QUERY_RETRIES: usize = 3;

/// Our own active paths, in QDC order. A path is ours when its target's adapter interface equals
/// `adapter_interface`. Only that comparison is made for foreign adapters.
pub(crate) fn own_paths(adapter_interface: &str) -> Result<Vec<OwnPath>, TwinError> {
    if adapter_interface.is_empty() {
        return Err(TwinError::Protocol("adapter interface is empty"));
    }
    let (paths, modes) = active_paths()?;
    // One adapter read per distinct target LUID; the cached answer decides every path on it.
    let mut ownership: BTreeMap<u64, bool> = BTreeMap::new();
    let mut own = Vec::new();
    for path in &paths {
        let adapter = path.targetInfo.adapterId;
        let is_own = match ownership.entry(luid_bits(adapter)) {
            Entry::Occupied(known) => *known.get(),
            Entry::Vacant(slot) => *slot.insert(adapter_is_own(adapter, adapter_interface)?),
        };
        if is_own {
            // A path whose monitor is departing (after REMOVE or a closed lane) has no monitor
            // path yet or any more; it is not a live own twin.
            if let Some(own_path) = own_path(path, &modes)? {
                own.push(own_path);
            }
        }
    }
    Ok(own)
}

/// The DPI of the monitor at `rect` (desktop pixels). A never-shown 1x1 helper, created under a
/// scoped PMv2 context, reports the monitor it sits on. That monitor's bounds must equal `rect`,
/// so the DPI always belongs to the twin's own display.
pub(crate) fn monitor_dpi(rect: [i32; 4]) -> Result<u32, TwinError> {
    let [left, top, right, bottom] = rect;
    if left >= right || top >= bottom {
        return Err(TwinError::Protocol("twin rect is empty"));
    }
    let _scope = DpiScope::new()?;
    // SAFETY: a documented query for this module's own instance handle; no state change.
    let instance = unsafe { GetModuleHandleW(null()) };
    let class = wide("STATIC");
    let title = wide("");
    // SAFETY: creates a never-shown 1x1 popup at the rect's top-left corner, under this thread's
    // PMv2 context. No existing window or output is touched.
    let helper = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            left,
            top,
            1,
            1,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    if helper.is_null() {
        return Err(TwinError::Native("monitor DPI helper", last_error()));
    }
    // SAFETY: queries only the live helper created above; DEFAULTTONULL never picks another monitor.
    let observed = unsafe { MonitorFromWindow(helper, MONITOR_DEFAULTTONULL) };
    // SAFETY: a documented read on our own per-monitor-aware helper; zero means failure.
    let dpi = unsafe { GetDpiForWindow(helper) };
    // SAFETY: destroys only the helper created on this thread above.
    if unsafe { DestroyWindow(helper) } == 0 {
        return Err(TwinError::Native(
            "monitor DPI helper cleanup",
            last_error(),
        ));
    }
    if observed.is_null() {
        return Err(TwinError::NotOnDesktop);
    }
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: cbSize is MONITORINFO's exact size and `info` is a writable local.
    if unsafe { GetMonitorInfoW(observed, &mut info) } == 0 {
        return Err(TwinError::Native("monitor bounds", last_error()));
    }
    let bounds = info.rcMonitor;
    if [bounds.left, bounds.top, bounds.right, bounds.bottom] != rect {
        // The twin's rect is not where the desktop shows a monitor.
        return Err(TwinError::NotOnDesktop);
    }
    if dpi == 0 {
        return Err(TwinError::Native("monitor DPI", last_error()));
    }
    Ok(dpi)
}

/// Reads the active paths and modes, bounded and retried as `src/displays.rs` does.
fn active_paths() -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), TwinError>
{
    for _ in 0..=QUERY_RETRIES {
        let (mut path_count, mut mode_count) = (0u32, 0u32);
        // SAFETY: initialized count outputs; active paths only, no topology or system change.
        let status = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        };
        if status != ERROR_SUCCESS {
            return Err(TwinError::Native("display buffer sizes", status));
        }
        if path_count as usize > PATH_LIMIT || mode_count as usize > MODE_LIMIT {
            return Err(TwinError::Protocol("display topology exceeds its bound"));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count.max(1) as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count.max(1) as usize];
        // SAFETY: each array's length is at least the capacity its count passes in, and no
        // topology output is requested. The call writes back the counts it filled.
        let status = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                null_mut(),
            )
        };
        if status == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if status != ERROR_SUCCESS {
            return Err(TwinError::Native("display paths", status));
        }
        if path_count as usize > paths.len() || mode_count as usize > modes.len() {
            return Err(TwinError::Protocol(
                "display topology changed during the query",
            ));
        }
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        return Ok((paths, modes));
    }
    Err(TwinError::Native(
        "display paths",
        ERROR_INSUFFICIENT_BUFFER,
    ))
}

/// True when this target adapter's `adapterDevicePath` equals our adapter interface. This is the
/// only fact read from a foreign adapter.
fn adapter_is_own(adapter: LUID, interface: &str) -> Result<bool, TwinError> {
    let mut name = DISPLAYCONFIG_ADAPTER_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
            size: size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
            adapterId: adapter,
            id: 0,
        },
        ..Default::default()
    };
    // SAFETY: a correctly sized ADAPTER_NAME request for a numeric target LUID from QDC; read-only.
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut name.header) };
    if status != 0 {
        return Err(TwinError::Native("adapter identity", status as u32));
    }
    Ok(field_text(&name.adapterDevicePath).is_some_and(|path| interface_path_eq(&path, interface)))
}

/// Reads one of our paths: its identity, target monitor path, source GDI name and source rect.
fn own_path(
    path: &DISPLAYCONFIG_PATH_INFO,
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> Result<Option<OwnPath>, TwinError> {
    let adapter = path.targetInfo.adapterId;
    if luid_bits(path.sourceInfo.adapterId) != luid_bits(adapter) {
        return Err(TwinError::Protocol(
            "own source adapter differs from its target",
        ));
    }
    let Some(monitor_path) = target_monitor_path(path)? else {
        return Ok(None);
    };
    Ok(Some(OwnPath {
        luid: luid_bits(adapter),
        target: path.targetInfo.id,
        source: path.sourceInfo.id,
        monitor_path,
        gdi_name: source_gdi_name(path)?,
        rect: source_rect(path, modes)?,
    }))
}

fn target_monitor_path(path: &DISPLAYCONFIG_PATH_INFO) -> Result<Option<String>, TwinError> {
    let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: a correctly sized TARGET_NAME request for one of our own active targets. Only
    // monitorDevicePath is read; the friendly name and EDID fields are never touched.
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut target.header) };
    if status != 0 {
        return Err(TwinError::Native("own monitor identity", status as u32));
    }
    if field_text(&target.monitorDevicePath).is_some_and(|text| text.is_empty()) {
        return Ok(None);
    }
    own_text(&target.monitorDevicePath, "own monitor path is invalid").map(Some)
}

fn source_gdi_name(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String, TwinError> {
    let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: a correctly sized SOURCE_NAME request for our own source; `own_path` has checked
    // that the source's adapter is the target's.
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut source.header) };
    if status != 0 {
        return Err(TwinError::Native("own source name", status as u32));
    }
    own_text(&source.viewGdiDeviceName, "own source name is invalid")
}

/// The source's rect, from its mode. QDC runs without `QDC_VIRTUAL_MODE_AWARE`, so a path
/// source's active union member is `modeInfoIdx`, and a source mode's is `sourceMode`.
fn source_rect(
    path: &DISPLAYCONFIG_PATH_INFO,
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> Result<[i32; 4], TwinError> {
    // SAFETY: without QDC_VIRTUAL_MODE_AWARE, modeInfoIdx is the active union member.
    let index = unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize;
    let mode = modes.get(index).ok_or(TwinError::Protocol(
        "own source mode index is out of bounds",
    ))?;
    if mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
        || luid_bits(mode.adapterId) != luid_bits(path.sourceInfo.adapterId)
        || mode.id != path.sourceInfo.id
    {
        return Err(TwinError::Protocol("own source mode identity mismatch"));
    }
    // SAFETY: infoType was checked to be SOURCE, so the sourceMode union member is the active one.
    let source = unsafe { mode.Anonymous.sourceMode };
    let left = source.position.x;
    let top = source.position.y;
    let right = edge(left, source.width)?;
    let bottom = edge(top, source.height)?;
    if left >= right || top >= bottom {
        return Err(TwinError::Protocol("own source rect is empty"));
    }
    Ok([left, top, right, bottom])
}

fn edge(start: i32, length: u32) -> Result<i32, TwinError> {
    i32::try_from(length)
        .ok()
        .and_then(|length| start.checked_add(length))
        .ok_or(TwinError::Protocol("own source rect overflows"))
}

/// The text before the first NUL, or `None` when the field is unterminated or not UTF-16.
fn field_text(field: &[u16]) -> Option<String> {
    let end = field.iter().position(|&unit| unit == 0)?;
    String::from_utf16(&field[..end]).ok()
}

/// Our own text fields must be nonempty, terminated and free of control characters.
fn own_text(field: &[u16], what: &'static str) -> Result<String, TwinError> {
    field_text(field)
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
        .ok_or(TwinError::Protocol(what))
}

fn luid_bits(luid: LUID) -> u64 {
    u64::from(luid.LowPart) | (u64::from(luid.HighPart as u32) << 32)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn last_error() -> u32 {
    // SAFETY: reads this thread's last-error code; no state change.
    unsafe { GetLastError() }
}

/// Scopes the PMv2 context to one call. Drop restores the thread's previous context.
struct DpiScope(DPI_AWARENESS_CONTEXT);

impl DpiScope {
    fn new() -> Result<Self, TwinError> {
        // SAFETY: changes this thread's awareness context only; the previous one is kept for Drop.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.is_null() {
            return Err(TwinError::Native("PMv2 context", last_error()));
        }
        Ok(Self(previous))
    }
}

impl Drop for DpiScope {
    fn drop(&mut self) {
        // SAFETY: restores exactly the context captured by the successful setter on this thread.
        unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}
