//! The trait surface every OS backend implements (09 §1). OS-free: traits and data only, no
//! policy. Platform crates (`crosspane-platform-*`) implement these traits; the engine depends only
//! on them, so all decision logic runs unchanged in `crosspane-testkit` simulations.
//!
//! Frozen for Phase 1 by WP-0.6 (2026-10-01): sessions and the I/O gate, displays, permissions,
//! global hotkeys, input capture, key and pointer injection, key storage, link information and
//! overlays. The window-projection (E2) traits are added and frozen when Phase 2a starts; their
//! provisional shape is recorded in `docs/wp/WP-0.6.md`. Changing a frozen trait is its own work
//! package (11).
//!
//! # Conventions for every trait
//!
//! - **Threads.** `Send` applies to the command handle. Native objects stay on the thread their OS
//!   requires (an AppKit run loop, a Wayland event queue), and the handle marshals calls there.
//! - **Events** go to the [`EventSink`] given to `subscribe`, which is called once. A subscription
//!   first delivers the current state (where the trait has one), then changes in the order they
//!   were observed. See [`EventSink`] for the delivery rules.
//! - **Time.** Every `at: MonoTime` is on this node's monotonic clock (the same clock the engine
//!   reads). Backends convert OS timestamps (e.g. Wayland milliseconds) and never pass them
//!   through raw.
//! - **Bounded calls.** Methods on the input path (capture, injection, overlays, hotkeys) return
//!   within 50 ms; all others within 2 s. A wait that would take longer returns
//!   [`PlatformError::Timeout`]. Nothing blocks on user interaction.
//! - **The I/O gate.** Backends that capture or inject receive the node's [`IoGate`] at
//!   construction and obey it (see [`IoGate`]).
//! - **Logging.** Errors and logs never contain key contents (04 §7).

#![deny(unsafe_code)]

pub mod audio;
pub mod capture;
pub mod display;
pub mod error;
pub mod frame;
pub mod hotkey;
pub mod inject;
pub mod keystore;
pub mod link;
pub mod overlay;
pub mod permission;
pub mod session;
pub mod sink;
pub mod tray;
pub mod window;

pub use audio::{
    AudioCapture, AudioDeviceError, AudioEvent, AudioFormat, AudioHost, AudioKind, AudioPlayback,
    AudioStop, VirtualPorts,
};
pub use capture::{
    CaptureAbort, CaptureEvent, CaptureId, CapturePortal, CaptureStart, Edge, EndReason,
    InputCapture, MotionKind, PortalId,
};
pub use display::Displays;
pub use error::{Permission, PlatformError};
pub use frame::{
    CaptureTarget, CursorImage, Frame, FrameCapture, FrameEvent, FrameImage, NativeImage,
    StreamEndReason, StreamId,
};
pub use hotkey::{Chord, GlobalHotkeys, HotkeyEvent};
pub use inject::{KeyInjector, PointerInjector};
pub use keystore::KeyStore;
pub use link::{Interface, LinkClass, LinkInfo};
pub use overlay::{Overlay, OverlayAnchor, OverlayEvent, OverlayHost, OverlayId, Rgb8};
pub use permission::{PermissionState, Permissions};
pub use session::{IoGate, LockState, SessionEvent, SessionEvents, SessionState};
pub use sink::EventSink;
pub use tray::{TrayEvent, TrayHost, TrayItem, TrayItemId, TrayMenu, TrayState};
pub use window::{
    Parked, ParkingKind, WindowEvent, WindowInfo, WindowParking, WindowRole, WindowSource,
    WindowState,
};
