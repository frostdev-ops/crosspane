//! Key and pointer injection over an EIS socket from the RemoteDesktop portal (WP-G1.4).
//!
//! One worker thread owns the `reis` connection (sender role) and both devices; the two command
//! handles share it, so held modifiers apply to pointer events (09 §1 shared injection source).
//! Calls marshal to the worker and wait at most 45 ms (the frozen 50 ms input-path limit).
//!
//! **Connections.** [`EisSource::attach`] gives the worker an EIS socket (a new portal epoch); it
//! replaces any previous connection after releasing what that one held. [`EisSource::detach`]
//! (the portal session closed) drops it. Without a connection whose seat has a keyboard and a
//! pointer resumed, the source is not live: presses, motion and scrolling fail with
//! `PlatformError::Backend("no portal input session")`; releases succeed (the compositor releases
//! a removed device's keys and buttons, so nothing stays held).
//!
//! **Contract** (`crosspane_platform::inject`, quoted in the task): the [`IoGate`] is checked
//! immediately before every press, move or scroll; releases go through while it is closed. Ups
//! are owed until submitted; `release_all` and drop release everything held. Every press or
//! release is followed by an EIS `frame`. Keys use `hid_to_evdev`; the desktop supplies repeat.
//! Never log key contents.
//!
//! **Regions.** `move_to(display, position)` converts the device-pixel position to the desktop's
//! logical space with the display's `DisplayGeometry` (`logical_origin + position / scale`) from
//! the `displays` snapshot function, and sends absolute motion on the pointer device whose region
//! contains that point. No region containing it is `PlatformError::NotFound`.
//!
//! **Lock keys.** `lock_keys` reads Caps and Num lock from the keyboard's latest `modifiers` event
//! against its keymap (xkbcommon); `None` before the first one. `set_lock_keys` taps the lock key
//! (down, frame, up, frame) when the wanted value differs and that key isn't held by us.
//!
//! **Recovery.** A crashed process's EIS connection died with it and the compositor released its
//! devices, so `recover_keys`/`recover_buttons` release only what this source holds and return Ok.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use crosspane_platform::{IoGate, KeyInjector, PlatformError, PointerInjector};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::PointDevice;
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollDelta};

/// The displays snapshot used for region mapping (the platform's `Displays::displays`).
pub type DisplaysFn = Arc<dyn Fn() -> Vec<DisplayInfo> + Send + Sync>;

/// The shared injection source. Cloning gives another handle to the same worker.
#[derive(Clone, Debug)]
pub struct EisSource {}

/// The keyboard half.
#[derive(Debug)]
pub struct EisKeyInjector {}

/// The pointer half.
#[derive(Debug)]
pub struct EisPointerInjector {}

impl EisSource {
    /// Start the worker, not yet connected.
    pub fn new(
        gate: Arc<IoGate>,
        displays: DisplaysFn,
    ) -> Result<(EisSource, EisKeyInjector, EisPointerInjector), PlatformError> {
        let _ = (gate, displays);
        Err(PlatformError::Unsupported(
            "EIS injection not implemented yet",
        ))
    }

    /// Connect to a new EIS socket (handshake as a sender named "Crosspane", bind the seat's
    /// keyboard, pointer, absolute pointer, button and scroll capabilities). Returns once the
    /// handshake is done or failed (bounded: 2 s). Replaces the previous connection.
    pub fn attach(&self, fd: OwnedFd) -> Result<(), PlatformError> {
        let _ = fd;
        Err(PlatformError::Unsupported(
            "EIS injection not implemented yet",
        ))
    }

    /// Drop the connection (the portal session closed). Idempotent.
    pub fn detach(&self) {}

    /// Whether a connection with a resumed keyboard and pointer exists now.
    pub fn is_live(&self) -> bool {
        false
    }
}

impl KeyInjector for EisKeyInjector {
    fn key(&mut self, usage: HidUsage, down: bool) -> Result<(), PlatformError> {
        let _ = (usage, down);
        Err(PlatformError::Unsupported(
            "EIS injection not implemented yet",
        ))
    }

    fn lock_keys(&self) -> Result<LockKeys, PlatformError> {
        Err(PlatformError::Unsupported(
            "EIS injection not implemented yet",
        ))
    }

    fn set_lock_keys(&mut self, wanted: LockKeys) -> Result<(), PlatformError> {
        let _ = wanted;
        Err(PlatformError::Unsupported(
            "EIS injection not implemented yet",
        ))
    }

    fn release_all(&mut self) -> Result<(), PlatformError> {
        Ok(())
    }

    fn recover_keys(&mut self, keys: &[HidUsage]) -> Result<(), PlatformError> {
        let _ = keys;
        Ok(())
    }
}

impl PointerInjector for EisPointerInjector {
    fn move_to(&mut self, display: DisplayId, position: PointDevice) -> Result<(), PlatformError> {
        let _ = (display, position);
        Err(PlatformError::Unsupported(
            "EIS injection not implemented yet",
        ))
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        let _ = (button, down);
        Err(PlatformError::Unsupported(
            "EIS injection not implemented yet",
        ))
    }

    fn scroll(&mut self, delta: ScrollDelta) -> Result<(), PlatformError> {
        let _ = delta;
        Err(PlatformError::Unsupported(
            "EIS injection not implemented yet",
        ))
    }

    fn release_all(&mut self) -> Result<(), PlatformError> {
        Ok(())
    }

    fn recover_buttons(&mut self, buttons: &[MouseButton]) -> Result<(), PlatformError> {
        let _ = buttons;
        Ok(())
    }
}
