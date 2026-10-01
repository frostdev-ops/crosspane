//! Frame/damage types, tile hashing (CPU reference implementation), lossless tile codec, copy-rect
//! detection, hybrid scheduler, rate controller, and codec traits.

#![deny(unsafe_code)]

pub mod audio;
pub mod codec;
pub mod hybrid;
pub mod picture;
pub mod tiles;
pub mod wire;
