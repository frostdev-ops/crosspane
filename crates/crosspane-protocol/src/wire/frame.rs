//! Frame header and the incremental stream decoder. Implemented in WP-1.1.

use std::collections::VecDeque;

use super::{HEADER_LEN, WIRE_VERSION, WireError};

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
    buffered: VecDeque<u8>,
    error: Option<WireError>,
}

impl FrameDecoder {
    /// A decoder for a channel whose payloads may be at most `max_payload` bytes.
    pub fn new(max_payload: usize) -> Self {
        FrameDecoder {
            max_payload,
            buffered: VecDeque::new(),
            error: None,
        }
    }

    /// Append received bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.error.is_none() {
            self.buffered.extend(bytes);
        }
    }

    /// The next complete frame, `None` if more bytes are needed, or an error. After an error the
    /// stream is unusable and the caller closes the connection.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, WireError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        if self.buffered.len() < HEADER_LEN {
            return Ok(None);
        }

        let header: [u8; HEADER_LEN] = std::array::from_fn(|i| self.buffered[i]);
        let (kind, len) = match decode_header(&header, self.max_payload) {
            Ok(header) => header,
            Err(error) => {
                self.error = Some(error.clone());
                return Err(error);
            }
        };
        if self.buffered.len() - HEADER_LEN < len {
            return Ok(None);
        }

        // Front drains of a deque consume bytes without shifting the remaining stream.
        self.buffered.drain(..HEADER_LEN);
        let payload = self.buffered.drain(..len).collect();
        Ok(Some(Frame { kind, payload }))
    }
}

/// Validate the header before any allocation based on its untrusted length.
pub(super) fn decode_header(bytes: &[u8], max_payload: usize) -> Result<(u8, usize), WireError> {
    let header = bytes.get(..HEADER_LEN).ok_or(WireError::Truncated)?;
    if header[0] != WIRE_VERSION {
        return Err(WireError::BadVersion(header[0]));
    }
    if header[2] != 0 || header[3] != 0 {
        return Err(WireError::BadReserved);
    }
    let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if len > max_payload {
        return Err(WireError::TooLarge {
            len,
            max: max_payload,
        });
    }
    Ok((header[1], len))
}
