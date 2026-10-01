//! Colour-space tags. The MVP is SDR end to end and tags content with its source colour space (A9).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ColorSpace {
    #[default]
    Srgb,
    DisplayP3,
    Bt709,
}
