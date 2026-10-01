//! Frame header and the incremental stream decoder. Implemented in WP-1.1.

use super::WireError;

/// One decoded frame: its kind and payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub payload: Vec<u8>,
}

/// Splits a byte stream into frames, rejecting oversized payloads before allocating.
#[derive(Debug)]
pub struct FrameDecoder {
    max_payload: usize,
}

impl FrameDecoder {
    /// A decoder for a channel whose payloads may be at most `max_payload` bytes.
    pub fn new(max_payload: usize) -> Self {
        FrameDecoder { max_payload }
    }

    /// Append received bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        let _ = (bytes, self.max_payload);
    }

    /// The next complete frame, `None` if more bytes are needed, or an error. After an error the
    /// stream is unusable and the caller closes the connection.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, WireError> {
        Err(WireError::BadValue("not implemented (WP-1.1)"))
    }
}
