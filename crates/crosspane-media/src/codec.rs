//! Video codec traits for the hybrid encoder's motion path (03 §7, WP-2.14).
//!
//! H.264, 4:2:0, SDR, real time: no B-frames, no lookahead, one access unit out for every frame
//! in, in **Annex B** form (start codes; SPS and PPS before every IDR). Pixels in and out are BGRA8
//! (the capture and canvas format). Implementations live in the platform crates: VideoToolbox on
//! macOS, FFmpeg (NVENC, NVDEC, software fallbacks) on Linux.

use crosspane_types::geom::PixelSize;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    /// No working encoder or decoder on this machine (the caller stays on lossless tiles).
    #[error("video codec unavailable: {0}")]
    Unavailable(String),
    /// The codec failed on this frame; the caller requests a key frame and carries on.
    #[error("video codec failed: {0}")]
    Failed(String),
    #[error("bad codec input: {0}")]
    BadInput(&'static str),
}

/// What [`VideoEncoder::encode`] produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodedVideo {
    /// The access unit is an IDR (decodable on its own).
    pub key: bool,
}

/// A real-time H.264 encoder for one projection.
pub trait VideoEncoder: Send {
    /// Encode one frame. `pixels` is BGRA8 with `stride` bytes per row and `size` pixels. Odd sizes
    /// are allowed: the encoder pads to even dimensions by repeating the last column and row, the
    /// decoder returns that padded (coded) size, and the receiver crops to the size in the media
    /// frame header. `out` is cleared and gets exactly one Annex B access unit. `force_key` makes
    /// it an IDR; the first frame and the first after a size change are always IDRs.
    fn encode(
        &mut self,
        pixels: &[u8],
        stride: u32,
        size: PixelSize,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedVideo, CodecError>;
    /// Target bitrate in bits per second, from the next frame on.
    fn set_bitrate(&mut self, bits_per_second: u32);
    /// The backend, for logs (e.g. `h264_nvenc`, `libx264`, `VideoToolbox`).
    fn name(&self) -> &str;
}

/// A low-latency H.264 decoder for one projection.
pub trait VideoDecoder: Send {
    /// Decode one Annex B access unit. On success `out` is replaced by the frame as BGRA8 rows of
    /// `width * 4` bytes and its coded size is returned (even dimensions; see
    /// [`VideoEncoder::encode`]). A frame that depends on one the decoder hasn't seen is an error
    /// (the caller requests a key frame); an IDR always decodes on its own.
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<PixelSize, CodecError>;
    /// The backend, for logs.
    fn name(&self) -> &str;
}

/// Creates encoders and decoders, one per projection.
pub trait VideoCodecs: Send + Sync {
    /// An encoder for frames of `size` (it adapts if the size changes later) at `bits_per_second`
    /// and about `fps` frames per second.
    fn encoder(
        &self,
        size: PixelSize,
        bits_per_second: u32,
        fps: u32,
    ) -> Result<Box<dyn VideoEncoder>, CodecError>;
    fn decoder(&self) -> Result<Box<dyn VideoDecoder>, CodecError>;
}
