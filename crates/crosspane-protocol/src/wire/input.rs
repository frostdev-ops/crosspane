//! Hot-path encodings: input-stream messages and pointer datagrams. Implemented in WP-1.1.

use super::{Frame, WireError};
use crate::msg::{InputMessage, PointerMessage};

/// Append one framed input message to `out`.
pub fn encode_input(msg: &InputMessage, out: &mut Vec<u8>) -> Result<(), WireError> {
    let _ = (msg, out);
    Err(WireError::BadValue("not implemented (WP-1.1)"))
}

/// Decode an input-stream frame.
pub fn decode_input(frame: &Frame) -> Result<InputMessage, WireError> {
    let _ = frame;
    Err(WireError::BadValue("not implemented (WP-1.1)"))
}

/// Encode a pointer message as one complete datagram (header plus payload).
pub fn encode_pointer(msg: &PointerMessage) -> Result<Vec<u8>, WireError> {
    let _ = msg;
    Err(WireError::BadValue("not implemented (WP-1.1)"))
}

/// Decode one pointer datagram.
pub fn decode_pointer(datagram: &[u8]) -> Result<PointerMessage, WireError> {
    let _ = datagram;
    Err(WireError::BadValue("not implemented (WP-1.1)"))
}
