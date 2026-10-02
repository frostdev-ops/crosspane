//! Ordinary-user native adapters; no production adapter is constructed by the shell.
#[cfg(target_os = "linux")]
pub mod linux;
