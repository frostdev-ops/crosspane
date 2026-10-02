//! Hyprland (0.56+) backends.

pub mod capture;
pub mod cursor;
pub mod displays;
pub mod frame_capture;
pub mod home_bind;
pub mod hotkeys;
pub mod inject;
pub mod ipc;
pub mod mirror;
pub mod overlay;
pub mod parking;
pub mod windows;

pub use cursor::cursor_position;
