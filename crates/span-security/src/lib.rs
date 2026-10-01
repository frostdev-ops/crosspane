//! Identity, SAS pairing, trust store, permissions, and revocation. Key storage lives behind a
//! trait, with OS key-store implementations in platform crates.

#![deny(unsafe_code)]
