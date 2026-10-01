//! Identity, pairing, trust and permissions (04 §2–§4). OS-free.
//!
//! Key *storage* is behind `crosspane_platform::KeyStore` and lives in the platform crates; this
//! crate only defines what is stored and how it is checked. Cryptography uses audited primitives
//! from the rustls crypto provider (aws-lc-rs) only (09 §2).

#![deny(unsafe_code)]

pub mod pairing;
pub mod rng;
