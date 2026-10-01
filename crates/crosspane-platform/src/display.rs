//! The local displays (03 §5).

use std::sync::Arc;

use crosspane_types::display::DisplayInfo;

use crate::{EventSink, PlatformError};

/// Enumerates this node's displays and reports changes (hot-plug, resolution, scale, arrangement).
pub trait Displays: Send {
    /// A snapshot of every connected display. Virtual displays Crosspane created for parking are
    /// included; the caller tells them apart by ID.
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError>;

    /// Start delivering full snapshots: the current one first, then one after every change. If the
    /// backend loses its observation it delivers an empty snapshot rather than leaving stale data.
    fn subscribe(
        &mut self,
        sink: Arc<dyn EventSink<Vec<DisplayInfo>>>,
    ) -> Result<(), PlatformError>;
}
