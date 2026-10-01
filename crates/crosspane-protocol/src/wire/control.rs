//! Control-stream encoding (protobuf, schema in docs/wp/WP-1.2.md). Implemented in WP-1.2.

use super::{Frame, WireError};
use crate::msg::ControlMessage;

/// Append one framed control message to `out`.
pub fn encode_control(msg: &ControlMessage, out: &mut Vec<u8>) -> Result<(), WireError> {
    let _ = (msg, out);
    Err(WireError::BadValue("not implemented (WP-1.2)"))
}

/// Decode a control-stream frame.
pub fn decode_control(frame: &Frame) -> Result<ControlMessage, WireError> {
    let _ = frame;
    Err(WireError::BadValue("not implemented (WP-1.2)"))
}
