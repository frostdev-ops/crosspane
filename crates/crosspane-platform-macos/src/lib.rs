//! macOS adapters for event taps, CGEvent, AX, ScreenCaptureKit, VideoToolbox, IOSurface/Metal,
//! Keychain, and TCC preflight through objc2. An isolated opt-in private_vdisplay module covers
//! CGVirtualDisplay (D7).

#![cfg(target_os = "macos")]

pub mod capture;
pub mod clock;
pub mod displays;
pub mod inject;
pub mod keychain;
pub mod main_thread;
pub mod overlay;
pub mod permissions;
pub mod session;
