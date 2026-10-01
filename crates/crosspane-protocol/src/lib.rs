//! Crosspane's wire messages, OS-free and without I/O (03 §2, 09 §1).
//!
//! - [`msg`] is the **logical model**: the typed messages the engine, `crosspane-testkit` and
//!   `crosspane-transport` exchange. Frozen by the lead (WP-1.1/1.2 specs).
//! - [`link`] is the engine's view of one authenticated peer connection, implemented by
//!   `crosspane-transport` (QUIC) and `crosspane-testkit` (simulation).
//! - [`wire`] holds the codecs: fixed little-endian layouts for the hot path (input, pointer) and a
//!   schema-evolvable encoding for control messages. Implemented in WP-1.1 and WP-1.2.

#![deny(unsafe_code)]

pub mod audio;
pub mod link;
pub mod msg;
pub mod negotiate;
pub mod projection;
pub mod wire;

/// ALPN protocol ID: the major version (04 §3). A different major version is refused with a clear
/// "update Crosspane" error.
pub const ALPN: &[u8] = b"crosspane/1";

/// Minor protocol version, negotiated in [`msg::Hello`]: both sides use `min(local, remote)`.
pub const PROTOCOL_MINOR: u32 = 1;
