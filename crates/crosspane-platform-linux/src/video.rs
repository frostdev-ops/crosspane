//! Real-time Annex B H.264 through libavcodec (WP-2.14b).

use crosspane_media::codec::{CodecError, EncodedVideo, VideoCodecs, VideoDecoder, VideoEncoder};
use crosspane_types::geom::PixelSize;
use ffmpeg_next::{self as ffmpeg, codec, format::Pixel, frame, software::scaling};

const GOP: u32 = 100_000;

#[derive(Clone, Copy, Debug)]
enum Backend {
    Nvenc,
    X264,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Self::Nvenc => "h264_nvenc",
            Self::X264 => "libx264",
        }
    }
}

/// FFmpeg's available real-time encoders, preferring NVENC over libx264.
#[derive(Debug)]
pub struct FfmpegCodecs {
    preferred: Backend,
}

impl FfmpegCodecs {
    /// Initialise FFmpeg and probe a 256×256 session (NVENC refuses frames below its minimum size,
    /// e.g. 64×64). Neither encoder opening is `Unavailable`.
    pub fn new() -> Result<Self, CodecError> {
        ffmpeg::init().map_err(|error| CodecError::Unavailable(error.to_string()))?;
        let size = PixelSize::new(256, 256);
        match Session::new(Backend::Nvenc, size, 8_000_000, 30) {
            Ok(_) => Ok(Self {
                preferred: Backend::Nvenc,
            }),
            Err(nvenc) => match Session::new(Backend::X264, size, 8_000_000, 30) {
                Ok(_) => Ok(Self {
                    preferred: Backend::X264,
                }),
                Err(x264) => Err(CodecError::Unavailable(format!(
                    "h264_nvenc: {nvenc}; libx264: {x264}"
                ))),
            },
        }
    }
}

impl VideoCodecs for FfmpegCodecs {
    fn encoder(
        &self,
        size: PixelSize,
        bits_per_second: u32,
        fps: u32,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        let coded = coded_size(size)?;
        let (backend, session) = match Session::new(self.preferred, coded, bits_per_second, fps) {
            Ok(session) => (self.preferred, session),
            Err(nvenc) if matches!(self.preferred, Backend::Nvenc) => {
                let session =
                    Session::new(Backend::X264, coded, bits_per_second, fps).map_err(|x264| {
                        CodecError::Unavailable(format!("h264_nvenc: {nvenc}; libx264: {x264}"))
                    })?;
                (Backend::X264, session)
            }
            Err(error) => return Err(CodecError::Unavailable(error.to_string())),
        };
        Ok(Box::new(FfmpegEncoder {
            backend,
            size,
            bitrate: bits_per_second,
            fps,
            session: Some(session),
            rebuild: false,
            pts: 0,
        }))
    }

    fn decoder(&self) -> Result<Box<dyn VideoDecoder>, CodecError> {
        let codec = ffmpeg::decoder::find_by_name("h264")
            .ok_or_else(|| CodecError::Unavailable("h264 decoder not found".into()))?;
        let mut context = codec_context(codec)
            .map_err(|error| CodecError::Unavailable(error.to_string()))?
            .decoder();
        context.set_flags(codec::Flags::LOW_DELAY);
        context.set_threading(codec::threading::Config::kind(
            codec::threading::Type::Slice,
        ));
        context.check(codec::decoder::Check::EXPLODE | codec::decoder::Check::BITSTREAM);
        context.conceal(codec::decoder::Conceal::empty());
        let decoder = context
            .video()
            .map_err(|error| CodecError::Unavailable(error.to_string()))?;
        Ok(Box::new(FfmpegDecoder {
            decoder,
            scaler: None,
            needs_key: true,
            references: ReferenceSequence::new(),
        }))
    }
}

// swscale has no thread affinity. Unlike AVCodecContext, ffmpeg-next does not mark it Send.
struct Scaler(scaling::Context);

// SAFETY: Scaler exclusively owns its context; it can move between threads, and all access
// requires &mut self. No pointer or reference to that context escapes this module.
unsafe impl Send for Scaler {}

struct Session {
    encoder: codec::encoder::video::Encoder,
    scaler: Option<Scaler>,
}

impl Session {
    fn new(backend: Backend, size: PixelSize, bitrate: u32, fps: u32) -> Result<Self, CodecError> {
        if bitrate == 0 || fps == 0 || fps > i32::MAX as u32 {
            return Err(CodecError::Failed("invalid bitrate or frame rate".into()));
        }
        let codec = ffmpeg::encoder::find_by_name(backend.name())
            .ok_or_else(|| CodecError::Failed(format!("{} not found", backend.name())))?;
        let formats = codec.video().map_err(failed)?.formats();
        let input_format = match backend {
            Backend::Nvenc => {
                let formats: Vec<_> = formats.into_iter().flatten().collect();
                if formats.contains(&Pixel::BGRA) {
                    Pixel::BGRA
                } else if formats.contains(&Pixel::BGRZ) {
                    Pixel::BGRZ
                } else {
                    Pixel::YUV420P
                }
            }
            Backend::X264 => Pixel::YUV420P,
        };
        let mut context = codec_context(codec)?.encoder().video().map_err(failed)?;
        context.set_width(size.width);
        context.set_height(size.height);
        context.set_format(input_format);
        context.set_time_base((1, fps as i32));
        context.set_frame_rate(Some((fps as i32, 1)));
        context.set_bit_rate(bitrate as usize);
        context.set_max_bit_rate(bitrate as usize);
        context.set_gop(GOP);
        context.set_max_b_frames(0);
        // Never enable GLOBAL_HEADER: every IDR carries its SPS and PPS in-band.
        let mut options = ffmpeg::Dictionary::new();
        options.set("bf", "0");
        options.set("forced-idr", "1");
        // Bound bitrate excursions without introducing lookahead or buffering frames.
        options.set(
            "bufsize",
            &(u64::from(bitrate).div_ceil(u64::from(fps)) * 2).to_string(),
        );
        match backend {
            Backend::Nvenc => {
                for (key, value) in [
                    ("preset", "p1"),
                    ("tune", "ull"),
                    ("zerolatency", "1"),
                    ("delay", "0"),
                    // VBR capped at the target: CBR pads easy frames with filler data.
                    ("rc", "vbr"),
                    ("rc-lookahead", "0"),
                    ("rgb_mode", "yuv420"),
                ] {
                    options.set(key, value);
                }
            }
            Backend::X264 => {
                options.set("preset", "ultrafast");
                options.set("tune", "zerolatency");
                options.set(
                    "x264-params",
                    "annexb=1:repeat-headers=1:scenecut=0:rc-lookahead=0:sync-lookahead=0",
                );
            }
        }
        let encoder = context.open_as_with(codec, options).map_err(failed)?;
        let scaler = if input_format == Pixel::YUV420P {
            Some(Scaler(
                scaling::Context::get(
                    Pixel::BGRA,
                    size.width,
                    size.height,
                    input_format,
                    size.width,
                    size.height,
                    scaling::Flags::BILINEAR,
                )
                .map_err(failed)?,
            ))
        } else {
            None
        };
        Ok(Self { encoder, scaler })
    }

    fn encode(
        &mut self,
        pixels: &[u8],
        stride: usize,
        size: PixelSize,
        key: bool,
        pts: i64,
        out: &mut Vec<u8>,
    ) -> Result<EncodedVideo, CodecError> {
        let coded = PixelSize::new(self.encoder.width(), self.encoder.height());
        let packed_format = if self.encoder.format() == Pixel::BGRZ {
            Pixel::BGRZ
        } else {
            Pixel::BGRA
        };
        let mut packed = video_frame(packed_format, coded)?;
        let destination_stride = packed.stride(0);
        let row_bytes = size.width as usize * 4;
        for y in 0..coded.height as usize {
            let source_y = y.min(size.height as usize - 1);
            let source = &pixels[source_y * stride..source_y * stride + row_bytes];
            let row = &mut packed.data_mut(0)[y * destination_stride..][..coded.width as usize * 4];
            row[..row_bytes].copy_from_slice(source);
            if coded.width != size.width {
                row[row_bytes..].copy_from_slice(&source[row_bytes - 4..]);
            }
        }
        let mut input = if let Some(scaler) = &mut self.scaler {
            let mut converted = video_frame(self.encoder.format(), coded)?;
            scaler.0.run(&packed, &mut converted).map_err(failed)?;
            converted
        } else {
            packed
        };
        input.set_pts(Some(pts));
        input.set_kind(if key {
            ffmpeg::picture::Type::I
        } else {
            ffmpeg::picture::Type::None
        });
        self.encoder.send_frame(&input).map_err(failed)?;
        let mut packet = ffmpeg::Packet::empty();
        self.encoder.receive_packet(&mut packet).map_err(failed)?;
        let bytes = packet
            .data()
            .filter(|bytes| !bytes.is_empty())
            .ok_or_else(|| CodecError::Failed("encoder returned an empty packet".into()))?;
        if !annex_b(bytes) {
            return Err(CodecError::Failed("encoder did not return Annex B".into()));
        }
        let idr = nal_types(bytes).any(|kind| kind == 5);
        if key && !idr {
            return Err(CodecError::Failed(
                "encoder did not honour forced IDR".into(),
            ));
        }
        if idr {
            let headers = nal_types(bytes)
                .take_while(|kind| *kind != 5)
                .fold(0, |mask, kind| {
                    mask | if kind == 7 {
                        1
                    } else if kind == 8 {
                        2
                    } else {
                        0
                    }
                });
            if headers != 3 {
                return Err(CodecError::Failed("IDR lacks inline SPS/PPS".into()));
            }
        }
        let mut extra = ffmpeg::Packet::empty();
        match self.encoder.receive_packet(&mut extra) {
            Err(error) if again(error) => (),
            Err(error) => return Err(failed(error)),
            Ok(()) => {
                return Err(CodecError::Failed(
                    "encoder returned multiple packets".into(),
                ));
            }
        }
        out.extend_from_slice(bytes);
        Ok(EncodedVideo { key: idr })
    }
}

struct FfmpegEncoder {
    backend: Backend,
    size: PixelSize,
    bitrate: u32,
    fps: u32,
    session: Option<Session>,
    rebuild: bool,
    pts: i64,
}

impl VideoEncoder for FfmpegEncoder {
    fn encode(
        &mut self,
        pixels: &[u8],
        stride: u32,
        size: PixelSize,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedVideo, CodecError> {
        out.clear();
        let coded = coded_size(size)?;
        let row_bytes = (size.width as usize)
            .checked_mul(4)
            .ok_or(CodecError::BadInput("row size overflow"))?;
        let stride = stride as usize;
        if stride < row_bytes {
            return Err(CodecError::BadInput("stride is smaller than a BGRA row"));
        }
        let length = (size.height as usize - 1)
            .checked_mul(stride)
            .and_then(|length| length.checked_add(row_bytes))
            .ok_or(CodecError::BadInput("pixel buffer length overflow"))?;
        if pixels.len() < length {
            return Err(CodecError::BadInput("pixel buffer is too short"));
        }
        if size != self.size || self.rebuild || self.session.is_none() {
            self.session = None;
            let session = match Session::new(self.backend, coded, self.bitrate, self.fps) {
                Ok(session) => session,
                // NVENC refuses some sizes (below its minimum): software for this size.
                Err(_) if matches!(self.backend, Backend::Nvenc) => {
                    self.backend = Backend::X264;
                    Session::new(Backend::X264, coded, self.bitrate, self.fps)?
                }
                Err(error) => return Err(error),
            };
            self.session = Some(session);
            self.size = size;
            self.rebuild = false;
            self.pts = 0;
        }
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| CodecError::Failed("encoder session missing".into()))?;
        let result = session.encode(
            pixels,
            stride,
            size,
            force_key || self.pts == 0,
            self.pts,
            out,
        );
        if result.is_err() {
            self.session = None;
        } else {
            self.pts = self.pts.saturating_add(1);
        }
        result
    }

    fn set_bitrate(&mut self, bits_per_second: u32) {
        if self.bitrate != bits_per_second {
            self.bitrate = bits_per_second;
            self.rebuild = true;
        }
    }

    fn name(&self) -> &str {
        self.backend.name()
    }
}

struct FfmpegDecoder {
    decoder: codec::decoder::Video,
    scaler: Option<Scaler>,
    needs_key: bool,
    references: ReferenceSequence,
}

impl FfmpegDecoder {
    fn frame(&mut self, data: &[u8]) -> Result<frame::Video, CodecError> {
        if data.is_empty() || data.len() > i32::MAX as usize || !annex_b(data) {
            return Err(CodecError::Failed("invalid Annex B access unit".into()));
        }
        let idr = nal_types(data).any(|kind| kind == 5);
        if self.needs_key && !idr {
            return Err(CodecError::Failed("decoder needs an IDR".into()));
        }
        if idr {
            // An IDR must also recover from an earlier malformed packet or different stream.
            self.decoder.flush();
        }
        self.references.check(data)?;
        let mut packet = ffmpeg::Packet::new(data.len());
        packet
            .data_mut()
            .ok_or_else(|| CodecError::Failed("packet allocation failed".into()))?
            .copy_from_slice(data);
        self.decoder.send_packet(&packet).map_err(failed)?;
        let mut decoded = frame::Video::empty();
        self.decoder.receive_frame(&mut decoded).map_err(failed)?;
        if decoded.is_corrupt() || decoded.has_decode_errors() {
            return Err(CodecError::Failed(
                "corrupt frame or missing reference".into(),
            ));
        }
        let mut extra = frame::Video::empty();
        match self.decoder.receive_frame(&mut extra) {
            Err(error) if again(error) => (),
            Err(error) => return Err(failed(error)),
            Ok(()) => {
                return Err(CodecError::Failed(
                    "access unit contains multiple frames".into(),
                ));
            }
        }
        self.needs_key = false;
        Ok(decoded)
    }

    fn convert(
        &mut self,
        decoded: &frame::Video,
        out: &mut Vec<u8>,
    ) -> Result<PixelSize, CodecError> {
        let size = PixelSize::new(decoded.width(), decoded.height());
        if coded_size(size)? != size {
            return Err(CodecError::Failed(
                "decoder returned an odd coded size".into(),
            ));
        }
        let definition = scaling::context::Definition {
            format: decoded.format(),
            width: size.width,
            height: size.height,
        };
        if self
            .scaler
            .as_ref()
            .is_none_or(|scaler| *scaler.0.input() != definition)
        {
            self.scaler = Some(Scaler(
                scaling::Context::get(
                    decoded.format(),
                    size.width,
                    size.height,
                    Pixel::BGRA,
                    size.width,
                    size.height,
                    scaling::Flags::BILINEAR,
                )
                .map_err(failed)?,
            ));
        }
        let mut bgra = video_frame(Pixel::BGRA, size)?;
        let scaler = self
            .scaler
            .as_mut()
            .ok_or_else(|| CodecError::Failed("decoder scaler missing".into()))?;
        scaler.0.run(decoded, &mut bgra).map_err(failed)?;
        let row_bytes = size.width as usize * 4;
        let length = row_bytes
            .checked_mul(size.height as usize)
            .ok_or_else(|| CodecError::Failed("decoded buffer size overflow".into()))?;
        out.clear();
        out.try_reserve(length)
            .map_err(|error| CodecError::Failed(error.to_string()))?;
        for row in bgra
            .data(0)
            .chunks_exact(bgra.stride(0))
            .take(size.height as usize)
        {
            out.extend_from_slice(&row[..row_bytes]);
        }
        Ok(size)
    }
}

impl VideoDecoder for FfmpegDecoder {
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<PixelSize, CodecError> {
        let result = self.frame(data).and_then(|frame| self.convert(&frame, out));
        if result.is_err() {
            self.decoder.flush();
            self.needs_key = true;
            self.references.last = None;
        }
        // All decode errors are per-frame failures, including invalid coded geometry.
        result.map_err(|error| match error {
            CodecError::Failed(_) => error,
            _ => CodecError::Failed(error.to_string()),
        })
    }

    fn name(&self) -> &str {
        "h264"
    }
}

fn coded_size(size: PixelSize) -> Result<PixelSize, CodecError> {
    if size.width == 0 || size.height == 0 {
        return Err(CodecError::BadInput("zero frame size"));
    }
    let width = size.width.checked_add(size.width % 2);
    let height = size.height.checked_add(size.height % 2);
    match (width, height) {
        (Some(width), Some(height)) if width <= i32::MAX as u32 && height <= i32::MAX as u32 => {
            Ok(PixelSize::new(width, height))
        }
        _ => Err(CodecError::BadInput("frame size exceeds FFmpeg dimensions")),
    }
}

fn codec_context(codec: ffmpeg::Codec) -> Result<codec::Context, CodecError> {
    let context = codec::Context::new_with_codec(codec);
    // SAFETY: inspecting the owned allocation's pointer does not dereference it.
    if unsafe { context.as_ptr().is_null() } {
        return Err(CodecError::Failed("codec context allocation failed".into()));
    }
    Ok(context)
}

fn video_frame(format: Pixel, size: PixelSize) -> Result<frame::Video, CodecError> {
    let mut frame = frame::Video::empty();
    // SAFETY: inspecting the owned allocation's pointer does not dereference it.
    if unsafe { frame.as_ptr().is_null() } {
        return Err(CodecError::Failed("frame allocation failed".into()));
    }
    frame.set_format(format);
    frame.set_width(size.width);
    frame.set_height(size.height);
    // SAFETY: this is a new, exclusively owned AVFrame with valid format and dimensions.
    // Use the FFI because ffmpeg-next's Video::new/alloc discard allocation errors.
    let result = unsafe { ffmpeg::ffi::av_frame_get_buffer(frame.as_mut_ptr(), 32) };
    if result < 0 {
        return Err(failed(ffmpeg::Error::from(result)));
    }
    Ok(frame)
}

fn annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

fn nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    // A four-byte start code also ends with a three-byte start code. Emulation prevention
    // prevents either sequence from occurring inside an H.264 NAL payload.
    data.windows(4)
        .filter(|window| window[..3] == [0, 0, 1])
        .map(|window| window[3] & 0x1f)
}

// FFmpeg synthesises missing reference pictures for frame_num gaps without necessarily setting
// AVFrame's decode_error_flags. Inspect only the parameter-set/slice prefixes needed to reject
// those gaps; all pixel decoding, complete header validation and reference lists stay in FFmpeg.
// The frozen format is progressive 8-bit 4:2:0 with no B-frames.
struct ReferenceSequence {
    frame_num_bits: [Option<u8>; 32],
    pps_sps: [Option<usize>; 256],
    last: Option<(usize, u32)>,
}

impl ReferenceSequence {
    fn new() -> Self {
        Self {
            frame_num_bits: [None; 32],
            pps_sps: [None; 256],
            last: None,
        }
    }

    fn check(&mut self, data: &[u8]) -> Result<(), CodecError> {
        self.prefixes(data)
            .ok_or_else(|| CodecError::Failed("invalid H.264 headers or missing reference".into()))
    }

    fn prefixes(&mut self, data: &[u8]) -> Option<()> {
        let mut picture = None;
        for nal in nals(data) {
            let header = *nal.first()?;
            let mut bits = Rbsp::new(&nal[1..]);
            match header & 0x1f {
                7 => {
                    let profile = bits.read(8)?;
                    bits.read(16)?; // constraint flags and level_idc
                    let sps = bits.ue()? as usize;
                    if matches!(
                        profile,
                        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
                    ) {
                        if bits.ue()? != 1 || bits.ue()? != 0 || bits.ue()? != 0 {
                            return None; // Only 8-bit 4:2:0 is in the frozen format.
                        }
                        bits.read(1)?; // qpprime_y_zero_transform_bypass_flag
                        if bits.read(1)? != 0 {
                            for list in 0..8 {
                                if bits.read(1)? != 0 {
                                    bits.scaling_list(if list < 6 { 16 } else { 64 })?;
                                }
                            }
                        }
                    }
                    let log2_minus4 = bits.ue()?;
                    if log2_minus4 > 12 {
                        return None;
                    }
                    *self.frame_num_bits.get_mut(sps)? = Some(log2_minus4 as u8 + 4);
                }
                8 => {
                    let pps = bits.ue()? as usize;
                    let sps = bits.ue()? as usize;
                    self.frame_num_bits.get(sps)?;
                    *self.pps_sps.get_mut(pps)? = Some(sps);
                }
                1 | 5 => {
                    bits.ue()?; // first_mb_in_slice (multiple slices share one frame_num)
                    let slice_type = bits.ue()?;
                    if slice_type > 9 || slice_type % 5 == 1 {
                        return None; // B-frames are outside the real-time contract.
                    }
                    let pps = bits.ue()? as usize;
                    let sps = self.pps_sps.get(pps).copied().flatten()?;
                    let count = self.frame_num_bits.get(sps).copied().flatten()?;
                    let number = bits.read(count)?;
                    let idr = header & 0x1f == 5;
                    let reference = header & 0x60 != 0;
                    let current = (sps, number, count, idr, reference);
                    if picture.is_some_and(|previous| previous != current) {
                        return None;
                    }
                    picture = Some(current);
                }
                _ => (),
            }
        }
        let (sps, number, count, idr, reference) = picture?;
        if idr {
            if number != 0 || !reference {
                return None;
            }
        } else {
            let (previous_sps, previous) = self.last?;
            if previous_sps != sps || number != (previous + 1) % (1 << count) {
                return None;
            }
        }
        if reference {
            self.last = Some((sps, number));
        }
        Some(())
    }
}

fn nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut remaining = data;
    std::iter::from_fn(move || {
        let start = remaining.windows(3).position(|bytes| bytes == [0, 0, 1])? + 3;
        remaining = &remaining[start..];
        let end = remaining
            .windows(3)
            .position(|bytes| bytes == [0, 0, 1])
            .unwrap_or(remaining.len());
        let nal = &remaining[..end];
        remaining = &remaining[end..];
        Some(nal)
    })
}

// Bounds-checked RBSP prefix reader, removing Annex B emulation-prevention bytes on demand.
struct Rbsp<'a> {
    bytes: std::slice::Iter<'a, u8>,
    byte: u8,
    left: u8,
    zeros: u8,
}

impl<'a> Rbsp<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes: bytes.iter(),
            byte: 0,
            left: 0,
            zeros: 0,
        }
    }

    fn read(&mut self, count: u8) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            if self.left == 0 {
                self.byte = *self.bytes.next()?;
                if self.zeros == 2 && self.byte == 3 {
                    self.zeros = 0;
                    self.byte = *self.bytes.next()?;
                }
                self.zeros = if self.byte == 0 {
                    (self.zeros + 1).min(2)
                } else {
                    0
                };
                self.left = 8;
            }
            self.left -= 1;
            value = (value << 1) | u32::from((self.byte >> self.left) & 1);
        }
        Some(value)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1_u32 << zeros) - 1 + self.read(zeros)?)
    }

    fn scaling_list(&mut self, length: usize) -> Option<()> {
        let mut last = 8;
        let mut next = 8;
        for _ in 0..length {
            if next != 0 {
                let code = i64::from(self.ue()?);
                let delta = if code % 2 == 0 {
                    -code / 2
                } else {
                    (code + 1) / 2
                };
                next = (last + delta).rem_euclid(256);
            }
            if next != 0 {
                last = next;
            }
        }
        Some(())
    }
}

fn again(error: ffmpeg::Error) -> bool {
    error
        == ffmpeg::Error::Other {
            errno: ffmpeg::error::EAGAIN,
        }
}

fn failed(error: ffmpeg::Error) -> CodecError {
    CodecError::Failed(error.to_string())
}
