//! Linux adapters (09 §1): Hyprland through its own protocols and IPC, logind and Secret Service
//! over D-Bus. Thin adapters implementing `crosspane-platform` traits: no policy.

#![cfg(target_os = "linux")]

pub mod hyprland;
pub mod logind;
pub mod permissions;
pub mod secret_service;
