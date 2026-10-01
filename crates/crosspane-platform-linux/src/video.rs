//! Real-time Annex B H.264 through libavcodec, with NVDEC / VA-API decoding.

use std::{ptr, sync::OnceLock};

use crosspane_media::codec::{CodecError, EncodedVideo, VideoCodecs, VideoDecoder, VideoEncoder};
use crosspane_media::picture::{Nv12, YuvColour, YuvMatrix};
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
    // Cache failures too: a forced, unavailable device must not be re-probed on every call.
    decoder_backend: OnceLock<Result<DecoderBackend, String>>,
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
                decoder_backend: OnceLock::new(),
            }),
            Err(nvenc) => match Session::new(Backend::X264, size, 8_000_000, 30) {
                Ok(_) => Ok(Self {
                    preferred: Backend::X264,
                    decoder_backend: OnceLock::new(),
                }),
                Err(x264) => Err(CodecError::Unavailable(format!(
                    "h264_nvenc: {nvenc}; libx264: {x264}"
                ))),
            },
        }
    }

    /// Open the selected decoder with access to its NV12 output method.
    pub fn decoder_nv12(&self) -> Result<FfmpegDecoder, CodecError> {
        let backend = self
            .decoder_backend
            .get_or_init(|| self.choose_decoder().map_err(|error| error.to_string()))
            .as_ref()
            .map_err(|error| CodecError::Unavailable(error.clone()))?;
        FfmpegDecoder::new(*backend)
    }

    fn choose_decoder(&self) -> Result<DecoderBackend, CodecError> {
        // Software decoding is the default: on the dev machine (RTX 3080 Ti, Ryzen 7800X3D) NVDEC
        // plus the transfer to system memory is slower than FFmpeg's software decoder (3440×1440:
        // 10.5 ms against 5.4 ms; WP-2.14d), and latency matters more than CPU here. `hardware`
        // takes the first working of CUDA and VA-API (for machines with weaker CPUs).
        let forced = match std::env::var("CROSSPANE_VIDEO_DECODER") {
            Ok(value) => match value.as_str() {
                "software" => Some(DecoderBackend::Software),
                "hardware" => None,
                "cuda" => Some(DecoderBackend::Cuda),
                "vaapi" => Some(DecoderBackend::Vaapi),
                _ => {
                    return Err(CodecError::Unavailable(
                        "CROSSPANE_VIDEO_DECODER must be software, hardware, cuda or vaapi".into(),
                    ));
                }
            },
            Err(std::env::VarError::NotPresent) => Some(DecoderBackend::Software),
            Err(error) => return Err(CodecError::Unavailable(error.to_string())),
        };
        if forced == Some(DecoderBackend::Software) {
            return Ok(DecoderBackend::Software);
        }

        // Use the existing encoder's low-delay IDR path. Four large, aligned colour blocks
        // check both geometry and pixel reconstruction without making lossy edges the probe.
        let size = PixelSize::new(256, 256);
        let mut source = Vec::with_capacity(256 * 256 * 4);
        for y in 0..256 {
            for x in 0..256 {
                let colour = match (x / 128, y / 128) {
                    (0, 0) => [48, 80, 112, 255],
                    (1, 0) => [112, 80, 48, 255],
                    (0, 1) => [96, 144, 192, 255],
                    _ => [192, 144, 96, 255],
                };
                source.extend_from_slice(&colour);
            }
        }
        let mut encoded = Vec::new();
        let probe = self.encoder(size, 8_000_000, 30).and_then(|mut encoder| {
            encoder
                .encode(&source, size.width * 4, size, true, &mut encoded)
                .map(|_| ())
        });
        if let Err(error) = probe {
            return if forced.is_some() {
                Err(CodecError::Unavailable(format!(
                    "hardware IDR probe: {error}"
                )))
            } else {
                Ok(DecoderBackend::Software)
            };
        }
        for backend in [DecoderBackend::Cuda, DecoderBackend::Vaapi] {
            if forced.is_some_and(|forced| forced != backend) {
                continue;
            }
            let result = FfmpegDecoder::new(backend).and_then(|mut decoder| {
                let mut pixels = Vec::new();
                let decoded_size = decoder.decode(&encoded, &mut pixels)?;
                if !decoder.hardware_frame || decoded_size != size || pixels.len() != source.len() {
                    return Err(CodecError::Failed(
                        "probe did not produce a hardware frame".into(),
                    ));
                }
                let mut squared_error = 0_u64;
                for (original, decoded) in source
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(pixels.as_chunks::<4>().0)
                {
                    if decoded[3] != 255 {
                        return Err(CodecError::Failed("probe returned incorrect alpha".into()));
                    }
                    for channel in 0..3 {
                        squared_error +=
                            u64::from(original[channel].abs_diff(decoded[channel])).pow(2);
                    }
                }
                // PSNR >= 30 dB against the known input (alpha is checked separately).
                if squared_error as f64 / (256.0 * 256.0 * 3.0) > 65.025 {
                    return Err(CodecError::Failed("probe returned incorrect pixels".into()));
                }
                Ok(())
            });
            match result {
                Ok(()) => return Ok(backend),
                Err(error) if forced.is_some() => {
                    return Err(CodecError::Unavailable(format!(
                        "{}: {error}",
                        backend.name()
                    )));
                }
                Err(_) => (),
            }
        }
        Ok(DecoderBackend::Software)
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
        Ok(Box::new(self.decoder_nv12()?))
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
        // NVENC's packed RGB conversion is BT.601 limited. FFmpeg's
        // nvenc_setup_h264_config also forces BT470BG/MPEG in the bitstream for RGB input.
        // Keep the context consistent with that conversion; planar input uses our BT.709 scaler.
        let matrix = if matches!(backend, Backend::Nvenc)
            && matches!(input_format, Pixel::BGRA | Pixel::BGRZ)
        {
            ffmpeg::color::Space::BT470BG
        } else {
            ffmpeg::color::Space::BT709
        };
        context.set_colorspace(matrix);
        context.set_color_range(ffmpeg::color::Range::MPEG);
        context.set_color_primaries(ffmpeg::color::Primaries::BT709);
        context
            .set_color_transfer_characteristic(ffmpeg::color::TransferCharacteristic::IEC61966_2_1);
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
        let mut scaler = if input_format == Pixel::YUV420P {
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
        if let Some(scaler) = &mut scaler {
            // SAFETY: the context is exclusively borrowed and live; sws_getCoefficients
            // returns static tables. RGB input is full range, YUV output is BT.709 limited.
            let result = unsafe {
                let coefficients = ffmpeg::ffi::sws_getCoefficients(ffmpeg::ffi::SWS_CS_ITU709);
                ffmpeg::ffi::sws_setColorspaceDetails(
                    scaler.0.as_mut_ptr(),
                    coefficients,
                    1,
                    coefficients,
                    0,
                    0,
                    1 << 16,
                    1 << 16,
                )
            };
            if result < 0 {
                return Err(failed(ffmpeg::Error::from(result)));
            }
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecoderBackend {
    Cuda,
    Vaapi,
    Software,
}

impl DecoderBackend {
    fn name(self) -> &'static str {
        match self {
            Self::Cuda => "h264 (cuda)",
            Self::Vaapi => "h264 (vaapi)",
            Self::Software => "h264",
        }
    }

    fn pixel(self) -> Pixel {
        match self {
            Self::Cuda => Pixel::CUDA,
            Self::Vaapi => Pixel::VAAPI,
            Self::Software => Pixel::None,
        }
    }
}

fn open_decoder(backend: DecoderBackend) -> Result<codec::decoder::Video, CodecError> {
    let codec = ffmpeg::decoder::find_by_name("h264")
        .ok_or_else(|| CodecError::Unavailable("h264 decoder not found".into()))?;
    let mut context = codec_context(codec)?.decoder();
    context.set_flags(codec::Flags::LOW_DELAY);
    context.set_threading(codec::threading::Config::kind(
        codec::threading::Type::Slice,
    ));
    context.check(codec::decoder::Check::EXPLODE | codec::decoder::Check::BITSTREAM);
    context.conceal(codec::decoder::Conceal::empty());
    if backend != DecoderBackend::Software {
        let device_type = match backend {
            DecoderBackend::Cuda => ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            DecoderBackend::Vaapi => ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            DecoderBackend::Software => ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE,
        };
        // SAFETY: context exclusively owns a non-null AVCodecContext whose hw_device_ctx is
        // initially null. Create directly into that field, transferring the one AVBufferRef
        // to libavcodec: ffmpeg-next's Context::drop calls avcodec_free_context, which unrefs
        // it on every exit (including a failed open). FFmpeg creates/owns hw_frames_ctx and
        // its references; we never manually unref either field. Null device/options select
        // FFmpeg's defaults. The callbacks below have no borrowed state or thread affinity.
        let result = unsafe {
            let context = context.as_mut_ptr();
            (*context).get_format = Some(match backend {
                DecoderBackend::Cuda => cuda_format,
                _ => vaapi_format,
            });
            ffmpeg::ffi::av_hwdevice_ctx_create(
                &mut (*context).hw_device_ctx,
                device_type,
                ptr::null(),
                ptr::null_mut(),
                0,
            )
        };
        if result < 0 {
            return Err(failed(ffmpeg::Error::from(result)));
        }
    }
    context.video().map_err(failed)
}

// Both callbacks are panic-free by construction: no allocation, indexing, arithmetic that can
// overflow, or panicking Rust calls. FFmpeg supplies a valid, NONE-terminated format list.
unsafe extern "C" fn cuda_format(
    _context: *mut ffmpeg::ffi::AVCodecContext,
    formats: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    // SAFETY: forwards FFmpeg's format list unchanged to the selector.
    unsafe { choose_format(formats, ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_CUDA) }
}

unsafe extern "C" fn vaapi_format(
    _context: *mut ffmpeg::ffi::AVCodecContext,
    formats: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    // SAFETY: forwards FFmpeg's format list unchanged to the selector.
    unsafe { choose_format(formats, ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI) }
}

unsafe fn choose_format(
    mut formats: *const ffmpeg::ffi::AVPixelFormat,
    hardware: ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    let mut software = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    if formats.is_null() {
        return software;
    }
    // SAFETY: FFmpeg's callback contract guarantees readable formats through the NONE sentinel;
    // av_pix_fmt_desc_get returns a static descriptor (or null). We retain no pointers.
    unsafe {
        while *formats != ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            let format = *formats;
            if format == hardware {
                return hardware;
            }
            let descriptor = ffmpeg::ffi::av_pix_fmt_desc_get(format);
            if software == ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE
                && !descriptor.is_null()
                && (*descriptor).flags & ffmpeg::ffi::AV_PIX_FMT_FLAG_HWACCEL as u64 == 0
            {
                software = format;
            }
            formats = formats.add(1);
        }
    }
    software
}

/// H.264 decoder supporting both legacy BGRA and NV12 output.
pub struct FfmpegDecoder {
    // None only after permanent hardware fallback, until the next IDR opens software.
    decoder: Option<codec::decoder::Video>,
    backend: DecoderBackend,
    hardware_failures: u8,
    hardware_frame: bool,
    // NV12 → YUV420P for hardware frames, and then (or directly, for software) → BGRA.
    planarizer: Option<Scaler>,
    scaler: Option<Scaler>,
    needs_key: bool,
    references: ReferenceSequence,
}

impl std::fmt::Debug for FfmpegDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfmpegDecoder")
            .field("backend", &self.backend)
            .field("needs_key", &self.needs_key)
            .finish_non_exhaustive()
    }
}

impl FfmpegDecoder {
    fn new(backend: DecoderBackend) -> Result<Self, CodecError> {
        let decoder =
            open_decoder(backend).map_err(|error| CodecError::Unavailable(error.to_string()))?;
        Ok(Self {
            decoder: Some(decoder),
            backend,
            hardware_failures: 0,
            hardware_frame: false,
            planarizer: None,
            scaler: None,
            needs_key: true,
            references: ReferenceSequence::new(),
        })
    }

    fn check(&mut self, data: &[u8]) -> Result<(), CodecError> {
        if data.is_empty() || data.len() > i32::MAX as usize || !annex_b(data) {
            return Err(CodecError::Failed("invalid Annex B access unit".into()));
        }
        let idr = nal_types(data).any(|kind| kind == 5);
        if self.needs_key && !idr {
            return Err(CodecError::Failed("decoder needs an IDR".into()));
        }
        self.references.check(data)
    }

    fn frame(&mut self, data: &[u8]) -> Result<frame::Video, CodecError> {
        if self.decoder.is_none() {
            self.decoder = Some(open_decoder(self.backend)?);
        }
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| CodecError::Failed("decoder session missing".into()))?;
        if nal_types(data).any(|kind| kind == 5) {
            // An IDR must also recover from an earlier malformed packet or different stream.
            decoder.flush();
        }
        let mut packet = ffmpeg::Packet::new(data.len());
        packet
            .data_mut()
            .ok_or_else(|| CodecError::Failed("packet allocation failed".into()))?
            .copy_from_slice(data);
        decoder.send_packet(&packet).map_err(failed)?;
        let mut decoded = frame::Video::empty();
        decoder.receive_frame(&mut decoded).map_err(failed)?;
        if decoded.is_corrupt() || decoded.has_decode_errors() {
            return Err(CodecError::Failed(
                "corrupt frame or missing reference".into(),
            ));
        }
        let mut extra = frame::Video::empty();
        match decoder.receive_frame(&mut extra) {
            Err(error) if again(error) => (),
            Err(error) => return Err(failed(error)),
            Ok(()) => {
                return Err(CodecError::Failed(
                    "access unit contains multiple frames".into(),
                ));
            }
        }
        self.hardware_frame =
            self.backend != DecoderBackend::Software && decoded.format() == self.backend.pixel();
        if self.hardware_frame {
            let mut transferred = frame::Video::empty();
            // SAFETY: inspecting this owned frame's allocation does not dereference it.
            if unsafe { transferred.as_ptr().is_null() } {
                return Err(CodecError::Failed(
                    "transfer frame allocation failed".into(),
                ));
            }
            // SAFETY: decoded owns a hardware frame and its hw_frames_ctx reference for the
            // whole call. The empty destination lets FFmpeg allocate a compatible CPU format
            // (usually NV12). Each frame wrapper calls av_frame_free once, releasing all its
            // buffer/frames references, on success and failure alike.
            let result = unsafe {
                ffmpeg::ffi::av_hwframe_transfer_data(transferred.as_mut_ptr(), decoded.as_ptr(), 0)
            };
            if result < 0 {
                return Err(failed(ffmpeg::Error::from(result)));
            }
            // SAFETY: both frames are owned and live. Copy colour metadata and other
            // properties separately: av_hwframe_transfer_data only transfers the planes.
            let result = unsafe {
                ffmpeg::ffi::av_frame_copy_props(transferred.as_mut_ptr(), decoded.as_ptr())
            };
            if result < 0 {
                return Err(failed(ffmpeg::Error::from(result)));
            }
            Ok(transferred)
        } else {
            Ok(decoded)
        }
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
        let mut bgra = video_frame(Pixel::BGRA, size)?;
        let scaler = cached_scaler(
            &mut self.scaler,
            decoded.format(),
            Pixel::BGRA,
            size,
            scaling::Flags::BILINEAR,
        )?;
        // SAFETY: exclusively borrowed live scaler and static coefficient tables. Honour
        // signalled colour for the legacy BGRA path as well as the NV12 reference converter.
        let result = unsafe {
            let matrix = if decoded.color_space() == ffmpeg::color::Space::BT709 {
                ffmpeg::ffi::SWS_CS_ITU709
            } else {
                ffmpeg::ffi::SWS_CS_ITU601
            };
            let coefficients = ffmpeg::ffi::sws_getCoefficients(matrix);
            let full = decoded.color_range() == ffmpeg::color::Range::JPEG
                || decoded.format() == Pixel::YUVJ420P;
            ffmpeg::ffi::sws_setColorspaceDetails(
                scaler.0.as_mut_ptr(),
                coefficients,
                i32::from(full),
                coefficients,
                1,
                0,
                1 << 16,
                1 << 16,
            )
        };
        if result < 0 {
            return Err(failed(ffmpeg::Error::from(result)));
        }
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

impl FfmpegDecoder {
    fn decode_picture<T>(
        &mut self,
        data: &[u8],
        output: impl FnOnce(&mut Self, frame::Video) -> Result<T, CodecError>,
    ) -> Result<T, CodecError> {
        // Header/gap/needs-IDR rejections do not count as hardware failures: they never reach
        // libavcodec. A successful decode/transfer resets the consecutive failure count.
        let result = self.check(data).and_then(|()| match self.frame(data) {
            Ok(frame) => {
                self.hardware_failures = 0;
                output(self, frame)
            }
            Err(error) => {
                if self.backend != DecoderBackend::Software {
                    self.hardware_failures += 1;
                    if self.hardware_failures == 3 {
                        tracing::warn!(
                            backend = self.backend.name(),
                            "hardware H.264 decoding failed three times; switching to software, awaiting IDR"
                        );
                        // Drop the hardware context and its refs now. Open software lazily on
                        // the next IDR, so even an allocation failure cannot keep hardware alive.
                        self.decoder = None;
                        self.planarizer = None;
                        self.scaler = None;
                        self.backend = DecoderBackend::Software;
                    }
                }
                Err(error)
            }
        });
        if result.is_err() {
            if let Some(decoder) = &mut self.decoder {
                decoder.flush();
            }
            self.needs_key = true;
            self.references.last = None;
        } else {
            self.needs_key = false;
        }
        // All decode errors are per-frame failures, including invalid coded geometry.
        result.map_err(|error| match error {
            CodecError::Failed(_) => error,
            _ => CodecError::Failed(error.to_string()),
        })
    }

    /// Decode into tightly packed planes: both strides equal the coded width.
    /// Existing plane allocations are reused. IDR recovery and errors match BGRA decoding.
    pub fn decode_nv12(&mut self, data: &[u8], out: &mut Nv12) -> Result<(), CodecError> {
        self.decode_picture(data, |_, frame| copy_nv12(&frame, out))
    }
}

impl VideoDecoder for FfmpegDecoder {
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<PixelSize, CodecError> {
        self.decode_picture(data, |decoder, frame| {
            let frame = planar_from_nv12(&mut decoder.planarizer, frame)?;
            decoder.convert(&frame, out)
        })
    }

    fn name(&self) -> &str {
        self.backend.name()
    }
}

fn copy_nv12(frame: &frame::Video, out: &mut Nv12) -> Result<(), CodecError> {
    let size = PixelSize::new(frame.width(), frame.height());
    if coded_size(size)? != size {
        return Err(CodecError::Failed(
            "decoder returned an odd coded size".into(),
        ));
    }
    let planar = match frame.format() {
        Pixel::YUV420P | Pixel::YUVJ420P => true,
        Pixel::NV12 => false,
        _ => {
            return Err(CodecError::Failed(
                "decoder returned unsupported NV12 source format".into(),
            ));
        }
    };
    let width = size.width as usize;
    let height = size.height as usize;
    let length = width
        .checked_mul(height)
        .ok_or_else(|| CodecError::Failed("decoded buffer size overflow".into()))?;
    // Validate the source rows before modifying output. FFmpeg normally supplies padded strides.
    let planes = [
        (0, height, width),
        (1, height / 2, if planar { width / 2 } else { width }),
        (2, height / 2, width / 2),
    ];
    for &(plane, rows, bytes) in &planes[..if planar { 3 } else { 2 }] {
        let stride = frame.stride(plane);
        let needed = (rows - 1)
            .checked_mul(stride)
            .and_then(|offset| offset.checked_add(bytes))
            .ok_or_else(|| CodecError::Failed("decoded plane size overflow".into()))?;
        if stride < bytes || frame.data(plane).len() < needed {
            return Err(CodecError::Failed(
                "decoded plane shorter than its size".into(),
            ));
        }
    }
    out.y
        .try_reserve(length.saturating_sub(out.y.len()))
        .map_err(|error| CodecError::Failed(error.to_string()))?;
    out.uv
        .try_reserve((length / 2).saturating_sub(out.uv.len()))
        .map_err(|error| CodecError::Failed(error.to_string()))?;
    out.y.resize(length, 0);
    out.uv.resize(length / 2, 0);
    for (row, dest) in out.y.chunks_exact_mut(width).enumerate() {
        dest.copy_from_slice(&frame.data(0)[row * frame.stride(0)..][..width]);
    }
    for (row, dest) in out.uv.chunks_exact_mut(width).enumerate() {
        if planar {
            let u = &frame.data(1)[row * frame.stride(1)..][..width / 2];
            let v = &frame.data(2)[row * frame.stride(2)..][..width / 2];
            for ((pair, u), v) in dest.as_chunks_mut::<2>().0.iter_mut().zip(u).zip(v) {
                pair.copy_from_slice(&[*u, *v]);
            }
        } else {
            dest.copy_from_slice(&frame.data(1)[row * frame.stride(1)..][..width]);
        }
    }
    out.size = size;
    out.y_stride = size.width;
    out.uv_stride = size.width;
    out.colour = YuvColour {
        matrix: match frame.color_space() {
            ffmpeg::color::Space::BT470BG | ffmpeg::color::Space::SMPTE170M => YuvMatrix::Bt601,
            _ => YuvMatrix::Bt709,
        },
        full_range: frame.color_range() == ffmpeg::color::Range::JPEG
            || frame.format() == Pixel::YUVJ420P,
    };
    Ok(())
}

// Why hardware output goes through planar YUV 4:2:0 before BGRA. H.264 decoding is bit-exact, so
// NVDEC's NV12 and the software decoder's YUV420P hold identical samples. swscale, however, takes
// a different route for the two inputs: unscaled YUV420P (even height, no ACCURATE_RND) uses the
// `yuv2rgb` converter, which replicates each chroma sample, while NV12 has no such converter and
// takes the general scaler, which interpolates chroma. The BGRA results differ by up to ~47 levels
// at coloured edges (about 2.5 dB on the test content). An unscaled NV12 → YUV420P conversion only
// deinterleaves the chroma (swscale's `nv12ToPlanarWrapper`, no arithmetic) and makes both decoders
// feed the same converter, so equal bitstreams give equal BGRA. The test
// `hardware_and_software_quality_agree` reports whether the frames are bit-identical.
//
// Frames that are not even-sized NV12 are returned untouched (`convert` converts any other format,
// and rejects odd coded sizes).
fn planar_from_nv12(
    slot: &mut Option<Scaler>,
    nv12: frame::Video,
) -> Result<frame::Video, CodecError> {
    let size = PixelSize::new(nv12.width(), nv12.height());
    if nv12.format() != Pixel::NV12
        || !size.width.is_multiple_of(2)
        || !size.height.is_multiple_of(2)
    {
        return Ok(nv12);
    }
    let mut planar = video_frame(Pixel::YUV420P, size)?;
    cached_scaler(
        slot,
        Pixel::NV12,
        Pixel::YUV420P,
        size,
        scaling::Flags::POINT,
    )?
    .0
    .run(&nv12, &mut planar)
    .map_err(failed)?;
    // SAFETY: both frames are live, independently owned frames; preserve the colour
    // description when changing only the chroma layout for legacy BGRA conversion.
    let result = unsafe { ffmpeg::ffi::av_frame_copy_props(planar.as_mut_ptr(), nv12.as_ptr()) };
    if result < 0 {
        return Err(failed(ffmpeg::Error::from(result)));
    }
    Ok(planar)
}

// A swscale context converting `from` → `to` at one size, rebuilt only when the input changes.
fn cached_scaler(
    slot: &mut Option<Scaler>,
    from: Pixel,
    to: Pixel,
    size: PixelSize,
    flags: scaling::Flags,
) -> Result<&mut Scaler, CodecError> {
    let definition = scaling::context::Definition {
        format: from,
        width: size.width,
        height: size.height,
    };
    if slot
        .as_ref()
        .is_none_or(|scaler| *scaler.0.input() != definition)
    {
        *slot = Some(Scaler(
            scaling::Context::get(
                from,
                size.width,
                size.height,
                to,
                size.width,
                size.height,
                flags,
            )
            .map_err(failed)?,
        ));
    }
    slot.as_mut()
        .ok_or_else(|| CodecError::Failed("scaler missing".into()))
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod nv12_tests {
    use super::*;
    use crosspane_media::picture::nv12_to_bgra;

    const PATCHES: [[u8; 4]; 12] = [
        [0, 0, 255, 255],
        [0, 255, 0, 255],
        [255, 0, 0, 255],
        [255, 255, 0, 255],
        [255, 0, 255, 255],
        [0, 255, 255, 255],
        [255, 255, 255, 255],
        [0, 0, 0, 255],
        [128, 128, 128, 255],
        [48, 80, 112, 255],
        [112, 80, 48, 255],
        [96, 144, 192, 255],
    ];

    fn patches(size: PixelSize, offset: usize) -> Vec<u8> {
        let mut source = Vec::new();
        for y in 0..size.height {
            for x in 0..size.width {
                source.extend_from_slice(&PATCHES[((y / 64 * 4 + x / 64) as usize + offset) % 12]);
            }
        }
        source
    }

    fn session(backend: Backend, size: PixelSize) -> Option<Session> {
        ffmpeg::init().unwrap();
        match Session::new(backend, size, 20_000_000, 30) {
            Ok(session) => Some(session),
            Err(error) if matches!(backend, Backend::Nvenc) => {
                eprintln!("SKIP h264_nvenc: {error}");
                None
            }
            Err(error) => panic!("libx264 must open: {error}"),
        }
    }

    fn hardware(backend: DecoderBackend, packet: &[u8]) -> Option<FfmpegDecoder> {
        match FfmpegDecoder::new(backend).and_then(|mut decoder| {
            decoder.decode_nv12(packet, &mut Nv12::default())?;
            if !decoder.hardware_frame {
                return Err(CodecError::Failed("no hardware frame produced".into()));
            }
            Ok(decoder)
        }) {
            Ok(decoder) => Some(decoder),
            Err(error) => {
                eprintln!("SKIP {}: {error}", backend.name());
                None
            }
        }
    }

    #[test]
    fn colour_round_trip() {
        for backend in [Backend::X264, Backend::Nvenc] {
            for size in [PixelSize::new(256, 256), PixelSize::new(250, 142)] {
                let Some(mut encoder) = session(backend, size) else {
                    continue;
                };
                let signalled = encoder.encoder.colorspace();
                let expected = if signalled == ffmpeg::color::Space::BT470BG {
                    YuvMatrix::Bt601
                } else {
                    YuvMatrix::Bt709
                };
                let mut software = FfmpegDecoder::new(DecoderBackend::Software).unwrap();
                let mut cuda = None;
                let mut errors = [[0_u8; 3]; 2];
                let mut packet = Vec::new();
                let mut picture = Nv12::default();
                // Rotate through all twelve colours even where fewer than twelve blocks fit.
                for index in 0..3 {
                    let source = patches(size, index * 4);
                    packet.clear();
                    encoder
                        .encode(
                            &source,
                            size.width as usize * 4,
                            size,
                            true,
                            index as i64,
                            &mut packet,
                        )
                        .unwrap();
                    if index == 0 {
                        cuda = hardware(DecoderBackend::Cuda, &packet);
                    }
                    for (pair, decoder) in std::iter::once(&mut software)
                        .chain(cuda.iter_mut())
                        .enumerate()
                    {
                        decoder.decode_nv12(&packet, &mut picture).unwrap();
                        assert_eq!(
                            picture.colour,
                            YuvColour {
                                matrix: expected,
                                full_range: false
                            }
                        );
                        assert_eq!(decoder.decoder.as_ref().unwrap().color_space(), signalled);
                        assert_eq!(
                            decoder.decoder.as_ref().unwrap().color_range(),
                            ffmpeg::color::Range::MPEG
                        );
                        assert_eq!(
                            decoder.decoder.as_ref().unwrap().color_primaries(),
                            ffmpeg::color::Primaries::BT709
                        );
                        assert_eq!(
                            decoder
                                .decoder
                                .as_ref()
                                .unwrap()
                                .color_transfer_characteristic(),
                            ffmpeg::color::TransferCharacteristic::IEC61966_2_1
                        );
                        let mut bgra = Vec::new();
                        nv12_to_bgra(&picture, size, &mut bgra).unwrap();
                        for top in (0..size.height).step_by(64) {
                            for left in (0..size.width).step_by(64) {
                                let x = left + (size.width - left).min(64) / 2;
                                let y = top + (size.height - top).min(64) / 2;
                                let at = (y * size.width + x) as usize * 4;
                                for channel in 0..3 {
                                    errors[pair][channel] = errors[pair][channel]
                                        .max(source[at + channel].abs_diff(bgra[at + channel]));
                                }
                            }
                        }
                    }
                }
                for (pair, error) in errors
                    .iter()
                    .enumerate()
                    .take(1 + usize::from(cuda.is_some()))
                {
                    eprintln!(
                        "WP-2.23 {} / {} {}x{} max B/G/R error {error:?}",
                        backend.name(),
                        if pair == 0 { "software" } else { "cuda" },
                        size.width,
                        size.height
                    );
                    assert!(error.iter().all(|error| *error <= 8));
                }
            }
        }
    }

    #[test]
    fn nv12_layout() {
        let size = PixelSize::new(250, 142);
        let mut encoder = session(Backend::X264, size).unwrap();
        let mut packet = Vec::new();
        encoder
            .encode(
                &patches(size, 0),
                size.width as usize * 4,
                size,
                true,
                0,
                &mut packet,
            )
            .unwrap();
        let mut software = FfmpegDecoder::new(DecoderBackend::Software).unwrap();
        let mut picture = Nv12::default();
        software.decode_nv12(&packet, &mut picture).unwrap();
        picture.validate().unwrap();
        assert_eq!(picture.y_stride, size.width);
        assert_eq!(picture.uv_stride, size.width);
        assert_eq!(picture.y.len(), (size.width * size.height) as usize);
        assert_eq!(picture.uv.len(), (size.width * size.height / 2) as usize);
        let y_allocation = picture.y.as_ptr();
        let uv_allocation = picture.uv.as_ptr();
        software.decode_nv12(&packet, &mut picture).unwrap();
        assert_eq!(picture.y.as_ptr(), y_allocation);
        assert_eq!(picture.uv.as_ptr(), uv_allocation);
        let raw = software.frame(&packet).unwrap();
        for y in 0..size.height as usize {
            assert_eq!(
                &picture.y[y * size.width as usize..][..size.width as usize],
                &raw.data(0)[y * raw.stride(0)..][..size.width as usize]
            );
        }
        for y in 0..size.height as usize / 2 {
            for x in 0..size.width as usize / 2 {
                assert_eq!(
                    picture.uv[y * size.width as usize + x * 2],
                    raw.data(1)[y * raw.stride(1) + x]
                );
                assert_eq!(
                    picture.uv[y * size.width as usize + x * 2 + 1],
                    raw.data(2)[y * raw.stride(2) + x]
                );
            }
        }
        for (format, space, range, matrix, full_range) in [
            (
                Pixel::NV12,
                ffmpeg::color::Space::BT709,
                ffmpeg::color::Range::MPEG,
                YuvMatrix::Bt709,
                false,
            ),
            (
                Pixel::NV12,
                ffmpeg::color::Space::BT470BG,
                ffmpeg::color::Range::JPEG,
                YuvMatrix::Bt601,
                true,
            ),
            (
                Pixel::YUV420P,
                ffmpeg::color::Space::SMPTE170M,
                ffmpeg::color::Range::MPEG,
                YuvMatrix::Bt601,
                false,
            ),
            (
                Pixel::YUVJ420P,
                ffmpeg::color::Space::Unspecified,
                ffmpeg::color::Range::Unspecified,
                YuvMatrix::Bt709,
                true,
            ),
            (
                Pixel::NV12,
                ffmpeg::color::Space::BT2020NCL,
                ffmpeg::color::Range::Unspecified,
                YuvMatrix::Bt709,
                false,
            ),
        ] {
            let mut fixture = video_frame(format, size).unwrap();
            fixture.set_color_space(space);
            fixture.set_color_range(range);
            let planar = format != Pixel::NV12;
            for (row, source) in picture.y.chunks_exact(size.width as usize).enumerate() {
                let stride = fixture.stride(0);
                fixture.data_mut(0)[row * stride..][..size.width as usize].copy_from_slice(source);
            }
            for (row, source) in picture.uv.chunks_exact(size.width as usize).enumerate() {
                if planar {
                    for (x, pair) in source.as_chunks::<2>().0.iter().enumerate() {
                        for (plane, value) in [(1, pair[0]), (2, pair[1])] {
                            let stride = fixture.stride(plane);
                            fixture.data_mut(plane)[row * stride + x] = value;
                        }
                    }
                } else {
                    let stride = fixture.stride(1);
                    fixture.data_mut(1)[row * stride..][..size.width as usize]
                        .copy_from_slice(source);
                }
            }
            let mut actual = Nv12::default();
            copy_nv12(&fixture, &mut actual).unwrap();
            assert_eq!(actual.y, picture.y);
            assert_eq!(actual.uv, picture.uv);
            assert_eq!(actual.colour, YuvColour { matrix, full_range });
        }
        for backend in [DecoderBackend::Cuda, DecoderBackend::Vaapi] {
            let Some(mut decoder) = hardware(backend, &packet) else {
                continue;
            };
            let mut actual = Nv12::default();
            decoder.decode_nv12(&packet, &mut actual).unwrap();
            assert!(decoder.hardware_frame);
            assert_eq!(actual.size, picture.size);
            assert_eq!(actual.colour, picture.colour);
            assert_eq!(actual.y.len(), picture.y.len());
            assert_eq!(actual.uv.len(), picture.uv.len());
            for (a, b) in actual
                .y
                .iter()
                .chain(&actual.uv)
                .zip(picture.y.iter().chain(&picture.uv))
            {
                assert!(a.abs_diff(*b) <= 2);
            }
        }
    }

    #[test]
    fn nv12_failure_and_size_change() {
        let mut decoder = FfmpegDecoder::new(DecoderBackend::Software).unwrap();
        let mut picture = Nv12::default();
        let mut packet = Vec::new();
        for size in [PixelSize::new(256, 256), PixelSize::new(250, 142)] {
            let mut encoder = session(Backend::X264, size).unwrap();
            let source = patches(size, 0);
            packet.clear();
            encoder
                .encode(&source, size.width as usize * 4, size, true, 0, &mut packet)
                .unwrap();
            let idr = packet.clone();
            packet.clear();
            encoder
                .encode(
                    &source,
                    size.width as usize * 4,
                    size,
                    false,
                    1,
                    &mut packet,
                )
                .unwrap();
            let mut fresh = FfmpegDecoder::new(DecoderBackend::Software).unwrap();
            assert!(matches!(
                fresh.decode_nv12(&packet, &mut picture),
                Err(CodecError::Failed(_))
            ));
            fresh.decode_nv12(&idr, &mut picture).unwrap();
            fresh.decode_nv12(&packet, &mut picture).unwrap();
            decoder.decode_nv12(&idr, &mut picture).unwrap();
            assert_eq!(picture.size, size);
            let previous = picture.clone();
            assert!(matches!(
                decoder.decode_nv12(&[], &mut picture),
                Err(CodecError::Failed(_))
            ));
            assert_eq!(previous, picture);
            assert!(decoder.decode_nv12(&packet, &mut picture).is_err());
            decoder.decode_nv12(&idr, &mut picture).unwrap();
        }
    }
}
