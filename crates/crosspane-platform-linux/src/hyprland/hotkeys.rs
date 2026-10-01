//! Global hotkeys on Hyprland. Not implemented yet: Hyprland offers no client protocol for a
//! global chord with press and release outside capture. Until a backend exists, this reports
//! `Unsupported` explicitly (the frozen contract forbids silently weakening the emergency
//! control); release and panic remain available from the tray and `crosspanectl`.

use std::sync::Arc;

use crosspane_platform::{Chord, EventSink, GlobalHotkeys, HotkeyEvent, PlatformError};

#[derive(Debug, Default)]
pub struct HyprlandHotkeys;

impl GlobalHotkeys for HyprlandHotkeys {
    fn set_chord(&mut self, _chord: &Chord) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("global hotkeys on Hyprland"))
    }

    fn subscribe(&mut self, _sink: Arc<dyn EventSink<HotkeyEvent>>) -> Result<(), PlatformError> {
        Ok(())
    }
}
