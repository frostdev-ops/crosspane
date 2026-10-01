//! Encodings (frozen formats in docs/wp/WP-1.1.md and docs/wp/WP-1.2.md).
//!
//! Every message travels in a frame with an 8-byte header:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 1 | wire version, [`WIRE_VERSION`] |
//! | 1 | 1 | kind (see the `KIND_*` constants) |
//! | 2 | 2 | reserved, must be zero |
//! | 4 | 4 | payload length, little-endian u32 |
//!
//! Streams carry frames back to back; a datagram carries exactly one frame. Receivers check the
//! length against the channel's cap *before* allocating (04 §3).

mod control;
mod frame;
mod input;

use thiserror::Error;

pub use control::{decode_control, encode_control};
pub use frame::{Frame, FrameDecoder};
pub use input::{decode_input, decode_pointer, encode_input, encode_pointer};

/// The frame layout version.
pub const WIRE_VERSION: u8 = 1;
/// Bytes in a frame header.
pub const HEADER_LEN: usize = 8;
/// Largest payload accepted on the input stream and in datagrams.
pub const MAX_INPUT_PAYLOAD: usize = 512;
/// Largest payload accepted on the control stream.
pub const MAX_CONTROL_PAYLOAD: usize = 1 << 20;

pub const KIND_KEY: u8 = 0x01;
pub const KIND_BUTTON: u8 = 0x02;
pub const KIND_SCROLL: u8 = 0x03;
pub const KIND_LOCK_KEYS: u8 = 0x04;
pub const KIND_STATE: u8 = 0x05;
pub const KIND_ACK: u8 = 0x06;
pub const KIND_STATUS: u8 = 0x07;
pub const KIND_POINTER: u8 = 0x20;
pub const KIND_CONTROL: u8 = 0x40;
/// Pairing messages on a pairing connection; the payload is encoded by `crosspane-security`.
pub const KIND_PAIRING: u8 = 0x60;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum WireError {
    /// Not enough bytes for a complete header or payload.
    #[error("truncated")]
    Truncated,
    /// The declared payload length exceeds the channel's cap.
    #[error("payload of {len} bytes exceeds the cap of {max}")]
    TooLarge { len: usize, max: usize },
    #[error("unsupported wire version {0}")]
    BadVersion(u8),
    #[error("unknown message kind {0:#04x}")]
    BadKind(u8),
    #[error("reserved header bytes are not zero")]
    BadReserved,
    /// The payload length doesn't match the kind's layout.
    #[error("wrong payload length {len} for kind {kind:#04x}")]
    BadLength { kind: u8, len: usize },
    /// A field holds a value the format doesn't allow (bad enum code, non-finite number, …).
    #[error("invalid value: {0}")]
    BadValue(&'static str),
    /// The control payload isn't valid protobuf for the schema.
    #[error("malformed control message")]
    BadControl,
    /// A control message from a newer peer that this version doesn't know; callers ignore it.
    #[error("unknown control message")]
    UnknownControl,
}
