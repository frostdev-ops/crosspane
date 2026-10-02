//! OS-free ten-millisecond Opus audio and bounded playout.
//!
//! Construction and packet ownership transfer happen on the audio worker, outside device callbacks.
//! No Debug implementation exposes samples or encoded payloads.

mod jitter;
pub use jitter::JitterBuffer;
mod resample;
pub use resample::{Processed, Resampler};
pub mod wire {
    pub use crosspane_protocol::audio::{AudioPacket, decode_audio, encode_audio};
}

use crosspane_types::audio::{
    AUDIO_FRAME_SAMPLES, AUDIO_RATE, AudioFormat, AudioKind, MAX_OPUS_PACKET,
};
use std::{fmt, time::Duration};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    #[error("invalid audio format")]
    InvalidFormat,
    #[error("invalid audio packet")]
    InvalidPacket,
    #[error("wrong audio stream")]
    WrongStream,
    #[error("audio codec failure")]
    Codec,
}

pub struct Encoder {
    codec: opus::Encoder,
    format: AudioFormat,
}

impl fmt::Debug for Encoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Encoder")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

fn channels(kind: AudioKind) -> opus::Channels {
    match kind {
        AudioKind::Speaker => opus::Channels::Stereo,
        AudioKind::Microphone => opus::Channels::Mono,
    }
}

impl Encoder {
    pub fn new(kind: AudioKind) -> Result<Self, AudioError> {
        let (application, bitrate) = match kind {
            AudioKind::Speaker => (opus::Application::LowDelay, 96_000),
            AudioKind::Microphone => (opus::Application::Voip, 32_000),
        };
        let mut codec = opus::Encoder::new(AUDIO_RATE, channels(kind), application)
            .map_err(|_| AudioError::Codec)?;
        codec
            .set_bitrate(opus::Bitrate::Bits(bitrate))
            .map_err(|_| AudioError::Codec)?;
        codec
            .set_vbr_constraint(true)
            .map_err(|_| AudioError::Codec)?;
        if kind == AudioKind::Microphone {
            codec.set_inband_fec(true).map_err(|_| AudioError::Codec)?;
            codec
                .set_packet_loss_perc(10)
                .map_err(|_| AudioError::Codec)?;
        }
        Ok(Self {
            codec,
            format: kind.format(),
        })
    }

    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, AudioError> {
        if pcm.len() != self.format.frame_samples()
            || pcm
                .iter()
                .any(|v| !v.is_finite() || !(-1.0..=1.0).contains(v))
        {
            return Err(AudioError::InvalidFormat);
        }
        self.codec
            .encode_vec_float(pcm, MAX_OPUS_PACKET)
            .map_err(|_| AudioError::Codec)
    }
}

pub struct Decoder {
    codec: opus::Decoder,
    format: AudioFormat,
}

impl fmt::Debug for Decoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decoder")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl Decoder {
    pub fn new(kind: AudioKind) -> Result<Self, AudioError> {
        Ok(Self {
            codec: opus::Decoder::new(AUDIO_RATE, channels(kind)).map_err(|_| AudioError::Codec)?,
            format: kind.format(),
        })
    }

    pub fn decode(
        &mut self,
        packet: Option<&[u8]>,
        fec: bool,
        pcm: &mut [f32],
    ) -> Result<(), AudioError> {
        pcm.fill(0.0);
        if pcm.len() != self.format.frame_samples() {
            return Err(AudioError::InvalidFormat);
        }
        if let Some(packet) = packet {
            validate_packet(packet)?;
        }
        let decoded = self
            .codec
            .decode_float(packet.unwrap_or(&[]), pcm, fec)
            .map_err(|_| AudioError::Codec);
        match decoded {
            Ok(AUDIO_FRAME_SAMPLES) if pcm.iter().all(|v| v.is_finite()) => Ok(()),
            _ => {
                pcm.fill(0.0);
                self.reset()?;
                Err(AudioError::Codec)
            }
        }
    }

    fn reset(&mut self) -> Result<(), AudioError> {
        self.codec.reset_state().map_err(|_| AudioError::Codec)
    }
}

fn validate_packet(packet: &[u8]) -> Result<(), AudioError> {
    if packet.is_empty()
        || packet.len() > MAX_OPUS_PACKET
        || opus::packet::get_nb_samples(packet, AUDIO_RATE).ok() != Some(AUDIO_FRAME_SAMPLES)
    {
        return Err(AudioError::InvalidPacket);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioPull {
    Waiting,
    Packet,
    /// A validated SILK/hybrid next-packet FEC decode attempt. Opus may internally use PLC
    /// when redundancy is absent; this is not proof of recovered audio.
    Fec,
    Concealed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioStats {
    pub target_delay: Duration,
    pub buffered_packets: usize,
    pub received: u64,
    pub lost: u64,
    pub concealed: u64,
    /// Next-packet FEC attempts, not confirmed redundancy recoveries.
    pub fec: u64,
    pub dropped: u64,
    pub drift_ppm: i32,
}
