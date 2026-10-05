//! Pure models behind the Win32 calls (WP-W0.2). No Win32 types or calls: native code (W1.x)
//! translates OS values into these inputs and carries out their outputs. Each model is
//! "builds and unit-tests its pure logic"; P9 verifies the Windows behaviour it assumes.

pub mod capture;
pub mod cursor;
pub mod displays;
pub mod frame_capture;
pub mod geometry;
pub mod hook;
pub mod hotkey;
pub mod inject;
pub mod journal;
pub mod keystore;
pub mod link;
pub mod overlay;
pub mod session;
pub mod tray;
#[cfg(feature = "video")]
pub mod video;
pub mod window;
pub mod winevent;
