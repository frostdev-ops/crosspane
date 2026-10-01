//! What a node reports about each of its displays (03 §5).

use serde::{Deserialize, Serialize};

use crate::color::ColorSpace;
use crate::geom::DisplayGeometry;
use crate::id::DisplayId;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: DisplayId,
    /// Human-readable name, e.g. the connector or model.
    pub name: String,
    pub geometry: DisplayGeometry,
    /// Refresh rate in millihertz (60 Hz = 60_000).
    pub refresh_millihz: u32,
    pub color_space: ColorSpace,
    pub hdr: bool,
}
