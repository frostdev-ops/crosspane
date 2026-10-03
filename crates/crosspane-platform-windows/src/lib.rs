//! Windows platform skeleton for Phase 3 Lane W.
//!
//! Cross-checking establishes buildability only. Runtime claims require testing on a real
//! Windows machine; this crate currently implements no native behaviour.

#![cfg(windows)]
#![deny(unsafe_code)]

pub mod stubs;

pub use stubs::UnsupportedWindows;
