//! A blocking client for the Crosspane Shell extension bridge, `io.frostdev.Crosspane.Shell1`
//! (WP-G1.2). Calls use a 2 s timeout (the frozen bound for non-input methods; overlay calls 45 ms).
//! An absent extension or a wrong `Version` is [`PlatformError::Unsupported`] at
//! [`ShellBridge::connect`]; a bus name that vanishes later makes calls fail with `Backend` and
//! subscribers get [`ShellEvent::Lost`].

use std::sync::Arc;

use crosspane_platform::PlatformError;

pub const BUS_NAME: &str = "io.frostdev.Crosspane.Shell";
pub const OBJECT_PATH: &str = "/io/frostdev/Crosspane/Shell";
pub const INTERFACE: &str = "io.frostdev.Crosspane.Shell1";
pub const VERSION: u32 = 1;

/// One window as the bridge reports it (`ListWindows`, frame rect in global logical coordinates).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellWindow {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub pid: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub focused: bool,
    pub minimized: bool,
    pub fullscreen: bool,
}

/// Events from the bridge, delivered on its signal thread in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellEvent {
    /// `WindowsChanged` for this epoch.
    WindowsChanged { epoch: u64 },
    /// `OverlayState`.
    OverlayState { id: u32, visible: bool },
    /// The bus name lost its owner or got a new one (Shell restart, extension disabled or
    /// re-enabled). Every window id and overlay from before is gone.
    Lost,
}

/// Called on the bridge's signal thread. Must not block.
pub type ShellCallback = Arc<dyn Fn(ShellEvent) + Send + Sync>;

/// A connection to the bridge on the session bus. Cloning shares the connection.
#[derive(Clone, Debug)]
pub struct ShellBridge {}

impl ShellBridge {
    /// Connect to the session bus, check the name has an owner and `Version == VERSION`, read
    /// `ShellEpoch`.
    pub fn connect() -> Result<ShellBridge, PlatformError> {
        Err(PlatformError::Unsupported(
            "Shell bridge not implemented yet",
        ))
    }

    /// The epoch read at connect (or the latest one after `Lost` handling by the caller).
    pub fn epoch(&self) -> u64 {
        0
    }

    /// Subscribe to `WindowsChanged`, `OverlayState` and name-owner changes. Called once.
    pub fn subscribe(&self, callback: ShellCallback) -> Result<(), PlatformError> {
        let _ = callback;
        Err(PlatformError::Unsupported(
            "Shell bridge not implemented yet",
        ))
    }

    /// `ListWindows`. An epoch different from [`ShellBridge::epoch`] is an error (`Backend`).
    pub fn list_windows(&self) -> Result<Vec<ShellWindow>, PlatformError> {
        Err(PlatformError::Unsupported(
            "Shell bridge not implemented yet",
        ))
    }

    /// `Activate`; `false` from the bridge is [`PlatformError::NotFound`].
    pub fn activate(&self, id: u64) -> Result<(), PlatformError> {
        let _ = id;
        Err(PlatformError::Unsupported(
            "Shell bridge not implemented yet",
        ))
    }

    /// `MoveResize`; `false` is [`PlatformError::NotFound`].
    pub fn move_resize(
        &self,
        id: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), PlatformError> {
        let _ = (id, x, y, width, height);
        Err(PlatformError::Unsupported(
            "Shell bridge not implemented yet",
        ))
    }

    /// `SetMinimized`; `false` is [`PlatformError::NotFound`].
    pub fn set_minimized(&self, id: u64, minimized: bool) -> Result<(), PlatformError> {
        let _ = (id, minimized);
        Err(PlatformError::Unsupported(
            "Shell bridge not implemented yet",
        ))
    }

    /// `Close`; `false` is [`PlatformError::NotFound`].
    pub fn close(&self, id: u64) -> Result<(), PlatformError> {
        let _ = id;
        Err(PlatformError::Unsupported(
            "Shell bridge not implemented yet",
        ))
    }

    /// `ShowOverlay` (45 ms timeout).
    pub fn show_overlay(
        &self,
        id: u32,
        x: i32,
        y: i32,
        anchor: u32,
        text: &str,
        accent: u32,
    ) -> Result<(), PlatformError> {
        let _ = (id, x, y, anchor, text, accent);
        Err(PlatformError::Unsupported(
            "Shell bridge not implemented yet",
        ))
    }

    /// `HideOverlay` (45 ms timeout).
    pub fn hide_overlay(&self, id: u32) -> Result<(), PlatformError> {
        let _ = id;
        Err(PlatformError::Unsupported(
            "Shell bridge not implemented yet",
        ))
    }
}
