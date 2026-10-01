//! Video codec traits for the hybrid encoder's motion path (03 §7, WP-2.14).
//!
//! H.264, 4:2:0, SDR, real time: no B-frames, no lookahead, one access unit out for every frame
//! in, in **Annex B** form (start codes; SPS and PPS before every IDR). Pixels in and out are BGRA8
//! (the capture and canvas format). Implementations live in the platform crates: VideoToolbox on
//! macOS, FFmpeg (NVENC, NVDEC, software fallbacks) on Linux.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crosspane_types::geom::PixelSize;

use crate::picture::{Decoded, Nv12, YuvColour};

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

    /// The pool this encoder takes native inputs from for frames of `size` (coded size: `size`
    /// rounded up to even), rebuilt when the size changes. `None` when the backend only takes CPU
    /// BGRA through [`VideoEncoder::encode`].
    fn input_pool(
        &mut self,
        size: PixelSize,
    ) -> Result<Option<Arc<dyn NativeInputPool>>, CodecError> {
        let _ = size;
        Ok(None)
    }

    /// Encode a buffer from [`VideoEncoder::input_pool`] that the caller has finished writing (its
    /// GPU work is complete). `size` is the frame's real size inside the buffer's coded size, as
    /// for [`VideoEncoder::encode`]; the rest is as there.
    fn encode_native(
        &mut self,
        input: &dyn NativeInput,
        size: PixelSize,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedVideo, CodecError> {
        let _ = (input, size, force_key, out);
        Err(CodecError::BadInput("native input not supported"))
    }
}

/// An encoder input buffer in the encoder's native memory (a VideoToolbox pool `CVPixelBuffer`, a
/// CUDA-mapped NV12 buffer), NV12 of the pool's coded size. The GPU code that fills it and the
/// encoder agree on the concrete type behind [`NativeInput::as_any`]. Dropping it returns the
/// buffer to its pool.
pub trait NativeInput: Send + Sync + fmt::Debug {
    fn size(&self) -> PixelSize;
    /// The colour description the encoder signals; the writer must produce exactly this.
    fn colour(&self) -> YuvColour;
    fn as_any(&self) -> &dyn Any;
}

/// Native input buffers of one coded size.
pub trait NativeInputPool: Send + Sync + fmt::Debug {
    /// A free buffer, or `Err(Failed)` when every buffer is in flight.
    fn acquire(&self) -> Result<Arc<dyn NativeInput>, CodecError>;
    fn size(&self) -> PixelSize;
    fn as_any(&self) -> &dyn Any;
}

/// A low-latency H.264 decoder for one projection.
pub trait VideoDecoder: Send {
    /// Decode one Annex B access unit. On success `out` is replaced by the frame as BGRA8 rows of
    /// `width * 4` bytes and its coded size is returned (even dimensions; see
    /// [`VideoEncoder::encode`]). A frame that depends on one the decoder hasn't seen is an error
    /// (the caller requests a key frame); an IDR always decodes on its own.
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<PixelSize, CodecError>;
    /// Decode one access unit like [`VideoDecoder::decode`], but into NV12 planes for the
    /// renderer's shader to convert (03 §6 video layer). `out`'s allocations are reused; on success
    /// it holds the coded size and the picture's colour description.
    fn decode_nv12(&mut self, data: &[u8], out: &mut Nv12) -> Result<(), CodecError>;
    /// The backend, for logs.
    fn name(&self) -> &str;

    /// Decode one access unit, leaving the picture in native memory when the backend can (no
    /// copy). Otherwise it decodes into `reuse` with [`VideoDecoder::decode_nv12`] (reusing its
    /// allocations when nothing else holds it) and returns it as [`Decoded::Nv12`].
    fn decode_native(&mut self, data: &[u8], reuse: &mut Arc<Nv12>) -> Result<Decoded, CodecError> {
        if Arc::get_mut(reuse).is_none() {
            *reuse = Arc::default();
        }
        let picture = Arc::get_mut(reuse).ok_or(CodecError::Failed("picture in use".into()))?;
        self.decode_nv12(data, picture)?;
        Ok(Decoded::Nv12(Arc::clone(reuse)))
    }
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
