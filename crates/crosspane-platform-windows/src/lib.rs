//! Windows platform adapters for Phase 3 Lane W.
//!
//! [`model`] holds the pure logic behind the Win32 calls (WP-W0.2). It builds and is unit-tested
//! on every host. Everything that touches Windows is `cfg(windows)`. Cross-checking establishes
//! buildability only; runtime claims require testing on a real Windows machine.

#![deny(unsafe_code)]

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod clock;
#[cfg(windows)]
pub mod displays;
#[cfg(windows)]
pub mod frame_capture;
#[cfg(windows)]
pub mod hotkey;
#[cfg(windows)]
pub mod inject;
pub mod keystore;
#[cfg(windows)]
pub mod link;
pub mod model;
#[cfg(windows)]
pub mod overlay;
#[cfg(windows)]
pub mod session;
#[cfg(windows)]
pub mod stubs;
#[cfg(windows)]
pub mod tray;
#[cfg(all(windows, feature = "video"))]
pub mod video;
#[cfg(windows)]
pub mod window;

#[cfg(windows)]
pub use stubs::UnsupportedWindows;
