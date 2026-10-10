//! Linux adapters (09 §1): Hyprland through its own protocols and IPC; GNOME and KDE through
//! portals, EIS and public Wayland protocols (GNOME also through the Crosspane Shell extension);
//! logind and Secret Service over D-Bus. Thin adapters implementing `crosspane-platform` traits: no policy.

#![cfg(target_os = "linux")]

pub mod audio;
pub mod desktop;
#[cfg(feature = "gpu")]
pub mod dmabuf;
pub mod gnome;
pub mod hyprland;
pub mod link;
pub mod logind;
pub mod permissions;
pub mod portal;
pub mod secret_service;
pub mod tray;
#[cfg(feature = "ffmpeg")]
pub mod video;
pub mod wayland_outputs;
