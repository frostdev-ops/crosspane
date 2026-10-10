//! GNOME-only adapters (WP-G0.1). Portal-based pieces shared with KDE live in `crate::portal`;
//! these need GNOME Shell itself: the Crosspane Shell extension's D-Bus bridge (owner-approved
//! 2026-10-09 as an isolated, opt-in module; interface frozen in
//! `packaging/gnome-shell-extension/crosspane@frostdev.io/io.frostdev.Crosspane.Shell1.xml`) and the
//! overlays drawn through it.

pub mod display_config;
pub mod overlay;
pub mod parking;
pub mod shell;
pub mod twin;
pub mod twin_parking;
pub mod window_capture;
pub mod windows;
