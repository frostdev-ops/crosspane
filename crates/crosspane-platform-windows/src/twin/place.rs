//! Moves a freshly added twin out of the user's reach (WP-W4.1d TWIN-ISO). Windows puts a new
//! IddCx monitor flush against the right edge of the desktop. `place_twin` re-positions only that
//! twin's source, to the corner `model::twin_place::twin_corner` picks, with `SetDisplayConfig`.
//!
//! The call hands the OS back the exact active path and mode arrays it just reported, with one
//! source position changed. Every other source keeps its position, size and format, and the call
//! asks for no changes (no `SDC_ALLOW_CHANGES`), so Windows refuses it instead of rewriting a real
//! monitor. It is validated first (`SDC_VALIDATE`, which changes nothing), and is not saved to the
//! CCD database (no `SDC_SAVE_TO_DATABASE`): the placement lasts for the session, as the twin does.
//! <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-setdisplayconfig>
#![allow(unsafe_code)]

use super::topology::{active_paths, luid_bits, own_paths, source_mode_index, source_rect};
use crate::model::{
    twin::{TwinDisplay, TwinError, same_path},
    twin_place::twin_corner,
};
use windows_sys::Win32::{Devices::Display::*, Foundation::POINTL};

/// Moves the twin to the isolated corner and returns the rect Windows then reports for it.
/// `Ok(None)` means nothing was changed: the twin shares its source with another path, or there is
/// no display to hang it on. `Ok(Some(rect))` is also returned, unchanged, when the twin already
/// sits at the chosen corner. An error leaves the desktop exactly as Windows placed it.
pub(super) fn place_twin(
    adapter_interface: &str,
    twin: &TwinDisplay,
) -> Result<Option<[i32; 4]>, TwinError> {
    let own = own_paths(adapter_interface)?;
    let mine = own
        .iter()
        .find(|path| {
            path.monitor_path.eq_ignore_ascii_case(&twin.monitor_path)
                && path.gdi_name.eq_ignore_ascii_case(&twin.gdi_name)
        })
        .ok_or(TwinError::NotOnDesktop)?;
    let (paths, mut modes) = active_paths()?;
    let index = paths
        .iter()
        .position(|path| {
            luid_bits(path.targetInfo.adapterId) == mine.luid && path.targetInfo.id == mine.target
        })
        .ok_or(TwinError::NotOnDesktop)?;
    let twin_path = &paths[index];
    let twin_source = (
        luid_bits(twin_path.sourceInfo.adapterId),
        twin_path.sourceInfo.id,
    );
    let mut others = Vec::with_capacity(paths.len());
    for (other_index, path) in paths.iter().enumerate() {
        if other_index == index {
            continue;
        }
        if (luid_bits(path.sourceInfo.adapterId), path.sourceInfo.id) == twin_source {
            // A cloned source moves with the twin; the placement would move a real monitor too.
            return Ok(None);
        }
        // A rect that cannot be read is an unknown display the twin might land on.
        others.push(source_rect(path, &modes)?);
    }
    let current = source_rect(twin_path, &modes)?;
    let width = u32::try_from(i64::from(current[2]) - i64::from(current[0]))
        .map_err(|_| TwinError::Protocol("twin rect is empty"))?;
    let height = u32::try_from(i64::from(current[3]) - i64::from(current[1]))
        .map_err(|_| TwinError::Protocol("twin rect is empty"))?;
    let Some([x, y]) = twin_corner(&others, width, height) else {
        return Ok(None);
    };
    if [x, y] == [current[0], current[1]] {
        return Ok(Some(current));
    }

    let slot = source_mode_index(twin_path, &modes)?;
    // `source_mode_index` checked that this mode is a SOURCE mode, so the sourceMode union member
    // is the active one. Only the twin's own position is written (a write to a `Copy` union field
    // needs no `unsafe`).
    modes[slot].Anonymous.sourceMode.position = POINTL { x, y };
    let path_count =
        u32::try_from(paths.len()).map_err(|_| TwinError::Protocol("too many paths"))?;
    let mode_count =
        u32::try_from(modes.len()).map_err(|_| TwinError::Protocol("too many modes"))?;
    for (flags, what) in [
        (
            SDC_VALIDATE | SDC_USE_SUPPLIED_DISPLAY_CONFIG,
            "twin placement check",
        ),
        (
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_NO_OPTIMIZATION,
            "twin placement",
        ),
    ] {
        // SAFETY: both arrays are the OS's own active paths and modes, at the lengths passed. The
        // first call only validates. The second applies the same arrays, in which only the twin's
        // source position differs from the active configuration.
        let status = unsafe {
            SetDisplayConfig(
                path_count,
                paths.as_ptr(),
                mode_count,
                modes.as_ptr(),
                flags,
            )
        };
        if status != 0 {
            return Err(TwinError::Native(what, status as u32));
        }
    }

    // What Windows reports now is the truth, whatever was asked for.
    own_paths(adapter_interface)?
        .into_iter()
        .find(|path| same_path(path, mine))
        .map(|path| Some(path.rect))
        .ok_or(TwinError::NotOnDesktop)
}
