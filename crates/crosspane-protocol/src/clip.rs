//! CLIP-v0 data-stream header: fetch u64 LE, kind u8 (1/2), length u32 LE, then exactly length
//! bytes of content. Validate the header before reading content. The receiver additionally checks
//! the expected fetch/kind and rejects extra bytes. This module never reads clipboard content.

use crosspane_types::ClipKind;

use crate::{msg::ClipFetchId, wire::WireError};

/// The protocol feature that enables the clipboard messages (`ClipOffer`, `ClipWithdraw`,
/// `ClipFetch`, `ClipFetchFailed`) and clip data streams (CLIP-v0 §4). Neither side sends them
/// unless both `Hello`s carry it.
pub const CLIP_FEATURE: &str = "clip/0";

pub const CLIP_DATA_HEADER_LEN: usize = 13;
pub const MAX_CLIP_TEXT: u32 = 1 << 20;
pub const MAX_CLIP_IMAGE: u32 = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipDataHeader {
    pub fetch: ClipFetchId,
    pub kind: ClipKind,
    pub len: u32,
}

fn check_len(header: ClipDataHeader) -> Result<(), WireError> {
    let max = match header.kind {
        ClipKind::Text => MAX_CLIP_TEXT,
        ClipKind::Image => MAX_CLIP_IMAGE,
    };
    if header.len > max {
        return Err(WireError::TooLarge {
            len: header.len as usize,
            max: max as usize,
        });
    }
    Ok(())
}

pub fn encode_clip_data_header(
    header: ClipDataHeader,
) -> Result<[u8; CLIP_DATA_HEADER_LEN], WireError> {
    check_len(header)?;
    let mut out = [0; CLIP_DATA_HEADER_LEN];
    out[..8].copy_from_slice(&header.fetch.0.to_le_bytes());
    out[8] = match header.kind {
        ClipKind::Text => 1,
        ClipKind::Image => 2,
    };
    out[9..].copy_from_slice(&header.len.to_le_bytes());
    Ok(out)
}

pub fn decode_clip_data_header(data: &[u8]) -> Result<ClipDataHeader, WireError> {
    if data.len() < CLIP_DATA_HEADER_LEN {
        return Err(WireError::Truncated);
    }
    if data.len() != CLIP_DATA_HEADER_LEN {
        return Err(WireError::BadValue("clipboard header length"));
    }
    let header = ClipDataHeader {
        fetch: ClipFetchId(u64::from_le_bytes(
            data[..8].try_into().map_err(|_| WireError::Truncated)?,
        )),
        kind: match data[8] {
            1 => ClipKind::Text,
            2 => ClipKind::Image,
            _ => return Err(WireError::BadValue("clipboard kind")),
        },
        len: u32::from_le_bytes(data[9..].try_into().map_err(|_| WireError::Truncated)?),
    };
    check_len(header)?;
    Ok(header)
}

#[cfg(test)]
mod tests {
    #[test]
    fn clipboard_feature_name_is_frozen() {
        assert_eq!(super::CLIP_FEATURE, "clip/0");
    }
}
