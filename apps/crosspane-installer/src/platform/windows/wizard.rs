//! The wizard's `live::Platform` implementation (L16) and the production controller `open()`.

use std::path::PathBuf;

/// The four native steps, in the order the screens show them (verbatim from `integration.rs`).
pub(crate) fn description() -> crate::live::PlatformDescription {
    super::integration::description()
}

/// Builds the font definitions and the production controller; `maintenance` opens the
/// maintenance screen.
pub(crate) fn open(
    payload: Option<PathBuf>,
    maintenance: bool,
) -> anyhow::Result<(
    eframe::egui::FontDefinitions,
    Box<dyn crate::gui::InstallerController>,
)> {
    let _ = (payload, maintenance);
    Err(anyhow::anyhow!("not built"))
}
