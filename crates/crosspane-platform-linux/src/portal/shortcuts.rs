//! The release chord through the GlobalShortcuts portal (WP-G1.5).
//!
//! A worker thread owns one GlobalShortcuts session (`ashpd`, async-io) with one shortcut,
//! id `crosspane-release`, whose preferred trigger is the chord spelled for the portal (e.g.
//! `CTRL+ALT+SHIFT+Escape`). `Activated` becomes `HotkeyEvent::Pressed`, `Deactivated` becomes
//! `Released`, timed with the injected [`Clock`] when the signal arrives.
//!
//! - **Consent.** Binding may show the desktop's shortcut dialog once; it runs on the worker,
//!   never inside a trait call. `set_chord` only records the chord and wakes the worker, then
//!   returns.
//! - **Pairing.** Exactly one `Released` per `Pressed`: a `Deactivated` without a preceding
//!   `Activated` is dropped; a lost session (closed, portal gone) while pressed emits `Released`.
//! - **Pre-held chord.** The portal can't report a chord already held at subscribe, so the first
//!   event is never synthesized. That is the safe direction: the engine re-arms only on a fresh
//!   press.
//! - **Injected input.** The engine never injects the chord (it consumes it during capture), so
//!   portal activations come from local input.
//! - No GlobalShortcuts portal: `set_chord` returns `PlatformError::Unsupported` (never a silent
//!   weakening). The tray and `crosspanectl` keep release and panic available.

use std::sync::Arc;

use crosspane_platform::{Chord, EventSink, GlobalHotkeys, HotkeyEvent, PlatformError};
use crosspane_types::time::Clock;

/// GlobalHotkeys through the GlobalShortcuts portal.
#[derive(Debug)]
pub struct PortalHotkeys {}

impl PortalHotkeys {
    /// Probe the portal (bounded: 2 s) and start the worker. No portal, or one without the
    /// GlobalShortcuts interface, is `Unsupported`.
    pub fn new(clock: Arc<dyn Clock>) -> Result<PortalHotkeys, PlatformError> {
        let _ = clock;
        Err(PlatformError::Unsupported(
            "GlobalShortcuts portal not implemented yet",
        ))
    }
}

impl GlobalHotkeys for PortalHotkeys {
    fn set_chord(&mut self, chord: &Chord) -> Result<(), PlatformError> {
        let _ = chord;
        Err(PlatformError::Unsupported(
            "GlobalShortcuts portal not implemented yet",
        ))
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<HotkeyEvent>>) -> Result<(), PlatformError> {
        let _ = sink;
        Ok(())
    }
}
