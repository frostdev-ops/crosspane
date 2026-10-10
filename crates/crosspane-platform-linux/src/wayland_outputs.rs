//! `Displays` from public Wayland protocols (WP-G1.1): `wl_output` (v4: name, mode, physical
//! size) plus `zxdg_output_manager_v1` (logical position and size). Compositor-neutral: used on
//! GNOME and KDE.
//!
//! - **Identity.** `DisplayId` is a stable hash of the output's `wl_output.name` (the connector,
//!   e.g. `DP-1`): FNV-1a 32-bit of the UTF-8 bytes. Two outputs with the same name, or an output
//!   without a name, make the snapshot fail (`Backend`), never a guessed id.
//! - **Geometry.** `pixel_size` is the current mode; `logical_origin` is the xdg-output logical
//!   position; `scale` is `mode width / logical width` (exact for fractional scaling, not the
//!   rounded `wl_output.scale`), with the transform applied (a 90°/270° rotation swaps the mode's
//!   width and height first). `physical_size` from `wl_output.geometry`, or 96 DPI from the pixel
//!   size when it reports 0.
//! - **Colour.** `ColorSpace::Srgb`, `hdr: false`.
//! - **Threading.** One worker thread owns its own Wayland connection and event queue. `displays`
//!   returns its latest complete snapshot (every output has `done` after its xdg-output). Changes
//!   (outputs added/removed, mode, scale, position) produce one new snapshot after a 100 ms
//!   debounce. A lost connection delivers an empty snapshot and the worker ends.

use std::sync::Arc;

use crosspane_platform::{Displays, EventSink, PlatformError};
use crosspane_types::display::DisplayInfo;

/// Displays from `wl_output` + `xdg-output`.
#[derive(Debug)]
pub struct WaylandOutputs {}

impl WaylandOutputs {
    /// Connect with `WAYLAND_DISPLAY`, bind the globals and wait for the first complete snapshot
    /// (bounded: 2 s). No xdg-output manager is `Unsupported`.
    pub fn new() -> Result<WaylandOutputs, PlatformError> {
        Err(PlatformError::Unsupported(
            "Wayland outputs not implemented yet",
        ))
    }
}

impl Displays for WaylandOutputs {
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
        Err(PlatformError::Unsupported(
            "Wayland outputs not implemented yet",
        ))
    }

    fn subscribe(
        &mut self,
        sink: Arc<dyn EventSink<Vec<DisplayInfo>>>,
    ) -> Result<(), PlatformError> {
        let _ = sink;
        Err(PlatformError::Unsupported(
            "Wayland outputs not implemented yet",
        ))
    }
}
