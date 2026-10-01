//! Linux adapters (09 §1): Hyprland through its own protocols and IPC, logind and Secret Service
//! over D-Bus. Thin adapters implementing `crosspane-platform` traits: no policy.

#![cfg(target_os = "linux")]

pub mod audio;
#[cfg(feature = "gpu")]
pub mod dmabuf;
pub mod hyprland;
pub mod link;
pub mod logind;
pub mod permissions;
pub mod secret_service;
pub mod tray;
#[cfg(feature = "ffmpeg")]
pub mod video;
