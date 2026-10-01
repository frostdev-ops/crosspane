//! Linux adapters for Hyprland IPC, twin-output parking and wlr/ext protocols, shared Wayland
//! client code, encode/decode backends, and Secret Service. Later modules cover portals (ashpd,
//! PipeWire, libei via reis) and X11.

#![cfg(target_os = "linux")]
