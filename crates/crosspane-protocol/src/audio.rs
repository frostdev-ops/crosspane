//! Bounded audio datagrams. Debug representations deliberately omit encoded samples.

use std::fmt;

use crate::wire::{HEADER_LEN, KIND_AUDIO, WIRE_VERSION, WireError};
pub use crosspane_types::audio::{AudioKind, AudioStreamId, MAX_OPUS_PACKET};

#[derive(Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub stream: AudioStreamId,
    pub seq: u32,
    /// Cumulative sender sample-frame count at 48 kHz, independently of wall-clock time.
    pub sample_time: u64,
    pub opus: Vec<u8>,
}

impl fmt::Debug for AudioPacket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioPacket")
            .field("stream", &self.stream)
            .field("seq", &self.seq)
            .field("sample_time", &self.sample_time)
            .field("encoded_len", &self.opus.len())
            .finish()
    }
}

pub fn encode_audio(packet: &AudioPacket) -> Result<Vec<u8>, WireError> {
    if packet.stream.0 == 0 || packet.opus.is_empty() || packet.opus.len() > MAX_OPUS_PACKET {
        return Err(WireError::BadValue("audio packet"));
    }
    let len = 14 + packet.opus.len();
    let mut out = Vec::with_capacity(HEADER_LEN + len);
    out.extend_from_slice(&[WIRE_VERSION, KIND_AUDIO, 0, 0]);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&packet.stream.0.to_le_bytes());
    out.extend_from_slice(&packet.seq.to_le_bytes());
    out.extend_from_slice(&packet.sample_time.to_le_bytes());
    out.extend_from_slice(&packet.opus);
    Ok(out)
}

pub fn decode_audio(data: &[u8]) -> Result<AudioPacket, WireError> {
    if data.len() < HEADER_LEN {
        return Err(WireError::Truncated);
    }
    if data[0] != WIRE_VERSION {
        return Err(WireError::BadVersion(data[0]));
    }
    if data[1] != KIND_AUDIO {
        return Err(WireError::BadKind(data[1]));
    }
    if data[2] != 0 || data[3] != 0 {
        return Err(WireError::BadReserved);
    }
    let array = |range: std::ops::Range<usize>| -> Result<[u8; 4], WireError> {
        data.get(range)
            .ok_or(WireError::Truncated)?
            .try_into()
            .map_err(|_| WireError::Truncated)
    };
    let len = u32::from_le_bytes(array(4..8)?) as usize;
    if len > 14 + MAX_OPUS_PACKET {
        return Err(WireError::TooLarge {
            len,
            max: 14 + MAX_OPUS_PACKET,
        });
    }
    if len <= 14 || data.len() != HEADER_LEN + len {
        return Err(WireError::BadLength {
            kind: KIND_AUDIO,
            len,
        });
    }
    let stream = AudioStreamId(u16::from_le_bytes(
        data[8..10].try_into().map_err(|_| WireError::Truncated)?,
    ));
    if stream.0 == 0 {
        return Err(WireError::BadValue("audio stream"));
    }
    Ok(AudioPacket {
        stream,
        seq: u32::from_le_bytes(array(10..14)?),
        sample_time: u64::from_le_bytes(data[14..22].try_into().map_err(|_| WireError::Truncated)?),
        opus: data[22..].to_vec(),
    })
}

/// Keep existing Grants messages compatible with peers which did not negotiate audio.
/// Callers must pass the negotiated feature intersection, not just the peer's advertisement.
pub fn grants_for_features(
    grants: &[crate::msg::Capability],
    negotiated_features: &[String],
) -> Vec<crate::msg::Capability> {
    let audio = negotiated_features.iter().any(|feature| feature == "audio");
    grants
        .iter()
        .copied()
        .filter(|cap| {
            audio
                || !matches!(
                    cap,
                    crate::msg::Capability::AudioSpeaker | crate::msg::Capability::AudioMic
                )
        })
        .collect()
}
