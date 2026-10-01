//! Synchronous, low-latency H.264 through the public VideoToolbox APIs.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::mpsc;

use crosspane_media::codec::{CodecError, EncodedVideo, VideoCodecs, VideoDecoder, VideoEncoder};
use crosspane_types::geom::PixelSize;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    CMBlockBuffer, CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescription,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMBlockBufferAssureMemoryNowFlag,
    kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetDataSize, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelFormatType_32BGRA,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTDecodeFrameFlags, VTDecodeInfoFlags,
    VTDecompressionOutputCallbackRecord, VTDecompressionSession, VTEncodeInfoFlags,
    VTSessionCopyProperty, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTDecompressionPropertyKey_RealTime,
    kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_High_AutoLevel,
    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

const START_CODE: [u8; 4] = [0, 0, 0, 1];
type EncodedFrame = Result<(EncodedVideo, Vec<u8>), CodecError>;
type DecodedFrame = Result<(PixelSize, Vec<u8>), CodecError>;

#[derive(Debug, Default)]
pub struct VtCodecs;

impl VtCodecs {
    pub fn new() -> Self {
        Self
    }
}

impl VideoCodecs for VtCodecs {
    fn encoder(
        &self,
        size: PixelSize,
        bits_per_second: u32,
        fps: u32,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        if fps == 0 || fps > i32::MAX as u32 || bits_per_second == 0 {
            return Err(CodecError::BadInput(
                "bitrate and frame rate must be positive",
            ));
        }
        Ok(Box::new(Encoder::new(size, bits_per_second, fps)?))
    }

    fn decoder(&self) -> Result<Box<dyn VideoDecoder>, CodecError> {
        Ok(Box::new(Decoder {
            sps: Vec::new(),
            pps: Vec::new(),
            session: None,
            last_reference: None,
        }))
    }
}

fn status(code: i32, operation: &str) -> Result<(), CodecError> {
    if code == 0 {
        Ok(())
    } else {
        Err(CodecError::Failed(format!("{operation}: OSStatus {code}")))
    }
}

fn missing(what: &str) -> CodecError {
    CodecError::Failed(format!("VideoToolbox returned no {what}"))
}

fn set(session: &CFType, key: &CFString, value: &CFType) -> Result<(), CodecError> {
    status(
        // SAFETY: Callers supply a live VT session and the documented CF value type for its property.
        unsafe { VTSessionSetProperty(session, key, Some(value)) },
        "VTSessionSetProperty",
    )
}

fn hardware(session: &CFType, key: &CFString) -> Result<bool, CodecError> {
    let mut value: *mut CFType = ptr::null_mut();
    status(
        // SAFETY: A live VT session and a boolean property; the out pointer receives an owned CFType.
        unsafe { VTSessionCopyProperty(session, key, None, ptr::from_mut(&mut value).cast()) },
        "VTSessionCopyProperty",
    )?;
    let value = NonNull::new(value).ok_or_else(|| missing("hardware property"))?;
    // SAFETY: CopyProperty transfers its +1 reference to this owner.
    let value = unsafe { CFRetained::from_raw(value) };
    value
        .downcast_ref::<CFBoolean>()
        .map(CFBoolean::as_bool)
        .ok_or_else(|| missing("boolean hardware property"))
}

fn coded_size(size: PixelSize) -> Result<PixelSize, CodecError> {
    let width = size.width.checked_add(1).map(|n| n & !1);
    let height = size.height.checked_add(1).map(|n| n & !1);
    match (width, height) {
        (Some(width), Some(height))
            if size.width > 0
                && size.height > 0
                && width <= i32::MAX as u32
                && height <= i32::MAX as u32
                && width.checked_mul(4).is_some()
                && (width as usize * 4)
                    .checked_mul(height as usize)
                    .is_some_and(|n| n <= isize::MAX as usize) =>
        {
            Ok(PixelSize::new(width, height))
        }
        _ => Err(CodecError::BadInput("invalid video dimensions")),
    }
}

struct Encoder {
    session: CFRetained<VTCompressionSession>,
    // Stable callback address, kept alive until after session invalidation in Drop.
    _callback: Box<mpsc::Sender<EncodedFrame>>,
    output: mpsc::Receiver<EncodedFrame>,
    size: PixelSize,
    bitrate: u32,
    fps: u32,
    frame: i64,
    first: bool,
}

// SAFETY: VT sessions have no thread affinity. &mut self serializes all use; callbacks access
// only the stable, thread-safe sender, never the session or other Encoder fields.
unsafe impl Send for Encoder {}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: A live owned session; invalidation ends callbacks before their sender is dropped.
        unsafe { self.session.invalidate() };
    }
}

impl Encoder {
    fn new(size: PixelSize, bitrate: u32, fps: u32) -> Result<Self, CodecError> {
        let coded = coded_size(size)?;
        let (tx, output) = mpsc::channel();
        let mut callback = Box::new(tx);
        let mut session = ptr::null_mut();
        // SAFETY: Immutable exported CFString constants; CFDictionary retains its boolean values.
        let specification = unsafe {
            CFDictionary::from_slices(
                &[
                    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
                ],
                &[CFBoolean::new(true), CFBoolean::new(true)],
            )
        };
        // SAFETY: Valid even dimensions, H.264, documented callback signature and a stable boxed
        // sender. The out pointer is writable; the specification remains live for this call.
        let code = unsafe {
            VTCompressionSession::create(
                None,
                coded.width as i32,
                coded.height as i32,
                kCMVideoCodecType_H264,
                Some(specification.as_ref()),
                None,
                None,
                Some(encoded_callback),
                ptr::from_mut(&mut *callback).cast(),
                NonNull::from(&mut session),
            )
        };
        if code != 0 {
            return Err(CodecError::Unavailable(format!(
                "VTCompressionSessionCreate: OSStatus {code}"
            )));
        }
        let session = NonNull::new(session).ok_or_else(|| missing("compression session"))?;
        // SAFETY: Successful Create transfers the session's +1 reference.
        let session = unsafe { CFRetained::from_raw(session) };
        let encoder = Self {
            session,
            _callback: callback,
            output,
            size,
            bitrate,
            fps,
            frame: 0,
            first: true,
        };
        // SAFETY: Immutable public property keys. Each value has the property's documented type.
        unsafe {
            set(
                encoder.session.as_ref(),
                kVTCompressionPropertyKey_RealTime,
                CFBoolean::new(true).as_ref(),
            )?;
            set(
                encoder.session.as_ref(),
                kVTCompressionPropertyKey_AllowFrameReordering,
                CFBoolean::new(false).as_ref(),
            )?;
            set(
                encoder.session.as_ref(),
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_High_AutoLevel.as_ref(),
            )?;
            set(
                encoder.session.as_ref(),
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                CFNumber::new_i32(i32::MAX).as_ref(),
            )?;
            set(
                encoder.session.as_ref(),
                kVTCompressionPropertyKey_ExpectedFrameRate,
                CFNumber::new_i32(fps as i32).as_ref(),
            )?;
            set(
                encoder.session.as_ref(),
                kVTCompressionPropertyKey_AverageBitRate,
                CFNumber::new_i64(i64::from(bitrate)).as_ref(),
            )?;
            status(
                encoder.session.prepare_to_encode_frames(),
                "VTCompressionSessionPrepareToEncodeFrames",
            )?;
        }
        Ok(encoder)
    }
}

impl VideoEncoder for Encoder {
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
        let row = size.width as usize * 4;
        let needed = (size.height as usize - 1)
            .checked_mul(stride as usize)
            .and_then(|n| n.checked_add(row))
            .ok_or(CodecError::BadInput("frame byte length overflow"))?;
        if (stride as usize) < row || pixels.len() < needed {
            return Err(CodecError::BadInput("short BGRA rows"));
        }
        if self.size != size {
            *self = Self::new(size, self.bitrate, self.fps)?;
        }
        if self.bitrate == 0 {
            return Err(CodecError::BadInput("bitrate must be positive"));
        }
        let image = input_buffer(pixels, stride as usize, size, coded)?;
        let force = force_key || self.first;
        // SAFETY: Immutable public property keys, with boolean/numeric values of the required types.
        let properties = unsafe {
            set(
                self.session.as_ref(),
                kVTCompressionPropertyKey_AverageBitRate,
                CFNumber::new_i64(i64::from(self.bitrate)).as_ref(),
            )?;
            CFDictionary::from_slices(
                &[kVTEncodeFrameOptionKey_ForceKeyFrame],
                &[CFBoolean::new(force)],
            )
        };
        let timestamp = CMTime {
            value: self.frame,
            timescale: self.fps as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let duration = CMTime {
            value: 1,
            ..timestamp
        };
        self.frame = self
            .frame
            .checked_add(1)
            .ok_or_else(|| CodecError::Failed("video timestamp overflow".into()))?;
        // SAFETY: Live session/image/properties and numeric timestamps. CompleteFrames waits for
        // this frame's callback, which sends only owned Rust data. No per-frame refcon is borrowed.
        let result = unsafe {
            status(
                self.session.encode_frame(
                    &image,
                    timestamp,
                    duration,
                    Some(properties.as_ref()),
                    ptr::null_mut(),
                    ptr::null_mut(),
                ),
                "VTCompressionSessionEncodeFrame",
            )
            .and_then(|()| {
                status(
                    self.session.complete_frames(timestamp),
                    "VTCompressionSessionCompleteFrames",
                )
            })
        }
        .and_then(|()| {
            self.output
                .try_recv()
                .map_err(|_| missing("encoded frame"))?
        })
        .and_then(|frame| {
            if self.output.try_recv().is_ok() {
                Err(CodecError::Failed(
                    "multiple encoded frames for one input".into(),
                ))
            } else if force && !frame.0.key {
                Err(CodecError::Failed(
                    "forced frame was not a key frame".into(),
                ))
            } else {
                Ok(frame)
            }
        });
        match result {
            Ok((encoded, bytes)) => {
                *out = bytes;
                self.first = false;
                Ok(encoded)
            }
            Err(error) => {
                // A failed/dropped frame must not leave callback output or references for the next
                // call. Recreate the session and force its first frame to IDR.
                *self = Self::new(size, self.bitrate, self.fps)?;
                Err(error)
            }
        }
    }

    fn set_bitrate(&mut self, bits_per_second: u32) {
        self.bitrate = bits_per_second;
    }
    fn name(&self) -> &str {
        // RequireHardwareAcceleratedVideoEncoder disallows software fallback. The low-latency
        // encoder does not expose UsingHardwareAcceleratedVideoEncoder on all macOS versions.
        "VideoToolbox (hardware required)"
    }
}

fn input_buffer(
    pixels: &[u8],
    stride: usize,
    size: PixelSize,
    coded: PixelSize,
) -> Result<CFRetained<CVPixelBuffer>, CodecError> {
    let mut image = ptr::null_mut();
    status(
        // SAFETY: Checked positive dimensions, documented BGRA format and writable output pointer.
        unsafe {
            CVPixelBufferCreate(
                None,
                coded.width as usize,
                coded.height as usize,
                kCVPixelFormatType_32BGRA,
                None,
                NonNull::from(&mut image),
            )
        },
        "CVPixelBufferCreate",
    )?;
    let image = NonNull::new(image).ok_or_else(|| missing("pixel buffer"))?;
    // SAFETY: Successful Create transfers its +1 reference to this owner.
    let image = unsafe { CFRetained::from_raw(image) };
    status(
        // SAFETY: Exclusively owned, nonplanar BGRA buffer; matching lock/unlock flags.
        unsafe { CVPixelBufferLockBaseAddress(&image, CVPixelBufferLockFlags::empty()) },
        "CVPixelBufferLockBaseAddress",
    )?;
    let copied = (|| {
        let destination_stride = CVPixelBufferGetBytesPerRow(&image);
        let len = CVPixelBufferGetDataSize(&image);
        let base = CVPixelBufferGetBaseAddress(&image).cast::<u8>();
        if base.is_null()
            || len > isize::MAX as usize
            || destination_stride < coded.width as usize * 4
            || destination_stride
                .checked_mul(coded.height as usize)
                .is_none_or(|n| n > len)
        {
            return Err(missing("valid BGRA storage"));
        }
        for y in 0..coded.height as usize {
            let source_start = y.min(size.height as usize - 1) * stride;
            let source = &pixels[source_start..source_start + size.width as usize * 4];
            // SAFETY: The new buffer does not alias the input. Its locked allocation's extent and
            // stride were checked above; each source row is validated before entering this helper.
            // Write directly so no Rust slice borrows potentially uninitialized CV row padding.
            unsafe {
                let target = base.add(y * destination_stride);
                ptr::copy_nonoverlapping(source.as_ptr(), target, source.len());
                if coded.width != size.width {
                    ptr::copy_nonoverlapping(
                        source.as_ptr().add(source.len() - 4),
                        target.add(source.len()),
                        4,
                    );
                }
            }
        }
        Ok(())
    })();
    // SAFETY: Balances the successful lock on all copy paths; no buffer slice remains borrowed.
    let unlocked = status(
        unsafe { CVPixelBufferUnlockBaseAddress(&image, CVPixelBufferLockFlags::empty()) },
        "CVPixelBufferUnlockBaseAddress",
    );
    copied?;
    unlocked?;
    Ok(image)
}

unsafe extern "C-unwind" fn encoded_callback(
    context: *mut c_void,
    _source: *mut c_void,
    code: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: The session's context is the boxed sender kept alive through invalidation; the
    // nullable sample is borrowed only during this callback, as specified by VideoToolbox.
    let (sender, sample) = unsafe {
        (
            &*context.cast::<mpsc::Sender<EncodedFrame>>(),
            sample.as_ref(),
        )
    };
    let result = status(code, "compression callback").and_then(|()| {
        if flags.contains(VTEncodeInfoFlags::FrameDropped) {
            return Err(CodecError::Failed(
                "VideoToolbox dropped the encoded frame".into(),
            ));
        }
        sample
            .ok_or_else(|| missing("encoded sample"))
            .and_then(annex_b_sample)
    });
    let _ = sender.send(result);
}

fn annex_b_sample(sample: &CMSampleBuffer) -> EncodedFrame {
    // SAFETY: A live output sample; these getters retain its immutable attachments/data/format.
    let (attachments, block, format) = unsafe {
        (
            sample.sample_attachments_array(false),
            sample.data_buffer(),
            sample.format_description(),
        )
    };
    let mut key = true; // Absence of NotSync is CoreMedia's sync-sample convention.
    if let Some(attachments) = attachments {
        // SAFETY: CoreMedia's attachment array contains CF dictionaries with CFString keys and CF values.
        let attachments = unsafe { &*ptr::from_ref(&*attachments).cast::<CFArray<CFType>>() };
        let first = attachments
            .get(0)
            .ok_or_else(|| missing("sample attachment"))?;
        let dictionary = first
            .downcast_ref::<CFDictionary>()
            .ok_or_else(|| missing("attachment dictionary"))?;
        // SAFETY: The dictionary was type-checked and its key/value types follow CoreMedia's contract.
        let dictionary =
            unsafe { &*ptr::from_ref(dictionary).cast::<CFDictionary<CFString, CFType>>() };
        // SAFETY: Immutable exported CFString key.
        if let Some(value) = dictionary.get(unsafe { kCMSampleAttachmentKey_NotSync }) {
            key = !value
                .downcast_ref::<CFBoolean>()
                .ok_or_else(|| missing("NotSync boolean"))?
                .as_bool();
        }
    }
    let mut out = Vec::new();
    if key {
        let format = format.ok_or_else(|| missing("H.264 format description"))?;
        for index in 0..2 {
            let mut bytes = ptr::null();
            let mut len = 0;
            let mut header_len = 0;
            status(
                // SAFETY: Live H.264 format; writable outputs. The returned parameter bytes remain
                // owned by the retained format and are copied before releasing it.
                unsafe {
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        &format,
                        index,
                        &mut bytes,
                        &mut len,
                        ptr::null_mut(),
                        &mut header_len,
                    )
                },
                "CMVideoFormatDescriptionGetH264ParameterSetAtIndex",
            )?;
            if bytes.is_null() || len == 0 || len > isize::MAX as usize || header_len != 4 {
                return Err(CodecError::Failed(
                    "invalid H.264 parameter set or NAL length size".into(),
                ));
            }
            out.extend_from_slice(&START_CODE);
            // SAFETY: The retained format supplies len live bytes, checked above.
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(bytes, len) });
        }
    }
    let block = block.ok_or_else(|| missing("AVCC buffer"))?;
    // SAFETY: Live retained block buffer; data_length reads its allocation extent.
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0; len];
    status(
        // SAFETY: The Vec has len initialized writable bytes. CopyDataBytes handles noncontiguous buffers.
        unsafe {
            block.copy_data_bytes(
                0,
                len,
                NonNull::new(avcc.as_mut_ptr().cast()).ok_or_else(|| missing("AVCC storage"))?,
            )
        },
        "CMBlockBufferCopyDataBytes",
    )?;
    let mut remaining = avcc.as_slice();
    while !remaining.is_empty() {
        let length = remaining
            .get(..4)
            .ok_or(CodecError::BadInput("truncated AVCC length"))?;
        let length = u32::from_be_bytes([length[0], length[1], length[2], length[3]]) as usize;
        remaining = &remaining[4..];
        if length == 0 || length > remaining.len() {
            return Err(CodecError::BadInput("invalid AVCC NAL length"));
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&remaining[..length]);
        remaining = &remaining[length..];
    }
    if avcc.is_empty() {
        return Err(missing("H.264 NAL units"));
    }
    Ok((EncodedVideo { key }, out))
}

struct Decoder {
    sps: Vec<u8>,
    pps: Vec<u8>,
    session: Option<DecodeSession>,
    last_reference: Option<u32>,
}

struct DecodeSession {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMVideoFormatDescription>,
    _callback: Box<mpsc::Sender<DecodedFrame>>,
    output: mpsc::Receiver<DecodedFrame>,
    hardware: bool,
    frame_num_bits: u32,
}

// SAFETY: VT and CoreMedia have no thread affinity. &mut Decoder serializes use; callbacks access
// only the stable sender. The format is immutable, and all returned image bytes are copied.
unsafe impl Send for DecodeSession {}

impl Drop for DecodeSession {
    fn drop(&mut self) {
        // SAFETY: This session is owned and every decode was synchronous; invalidation ends use of
        // its boxed callback context before the context is dropped.
        unsafe { self.session.invalidate() };
    }
}

impl DecodeSession {
    fn new(sps: &[u8], pps: &[u8]) -> Result<Self, CodecError> {
        let frame_num_bits = frame_num_bits(sps)?;
        let mut pointers = [NonNull::from(&sps[0]), NonNull::from(&pps[0])];
        let mut lengths = [sps.len(), pps.len()];
        let mut format = ptr::null();
        status(
            // SAFETY: Two nonempty parameter-set slices, pointers and lengths valid for this call,
            // writable output and four-byte AVCC lengths. CoreMedia copies the parameter sets.
            unsafe {
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    None,
                    2,
                    NonNull::from(&mut pointers[0]),
                    NonNull::from(&mut lengths[0]),
                    4,
                    NonNull::from(&mut format),
                )
            },
            "CMVideoFormatDescriptionCreateFromH264ParameterSets",
        )?;
        let format = NonNull::new(format.cast_mut()).ok_or_else(|| missing("decoder format"))?;
        // SAFETY: Successful Create transfers the format's +1 reference.
        let format = unsafe { CFRetained::from_raw(format) };
        let (tx, output) = mpsc::channel();
        let mut callback = Box::new(tx);
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(decoded_callback),
            decompressionOutputRefCon: ptr::from_mut(&mut *callback).cast(),
        };
        // SAFETY: Immutable public keys; dictionary values have their documented types.
        let (specification, attributes) = unsafe {
            (
                CFDictionary::from_slices(
                    &[kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder],
                    &[CFBoolean::new(true)],
                ),
                CFDictionary::from_slices(
                    &[kCVPixelBufferPixelFormatTypeKey],
                    &[&*CFNumber::new_i64(i64::from(kCVPixelFormatType_32BGRA))],
                ),
            )
        };
        let mut session = ptr::null_mut();
        // SAFETY: Retained valid H.264 format, BGRA destination attributes and correct callback.
        // VT copies the callback record; the boxed sender's address remains stable until invalidation.
        let code = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                Some(specification.as_ref()),
                Some(attributes.as_ref()),
                &record,
                NonNull::from(&mut session),
            )
        };
        if code != 0 {
            return Err(CodecError::Unavailable(format!(
                "VTDecompressionSessionCreate: OSStatus {code}"
            )));
        }
        let session = NonNull::new(session).ok_or_else(|| missing("decompression session"))?;
        // SAFETY: Successful Create transfers the session's +1 reference.
        let session = unsafe { CFRetained::from_raw(session) };
        let mut decoder = Self {
            session,
            format,
            _callback: callback,
            output,
            hardware: false,
            frame_num_bits,
        };
        // SAFETY: Immutable public keys and documented boolean property value.
        unsafe {
            set(
                decoder.session.as_ref(),
                kVTDecompressionPropertyKey_RealTime,
                CFBoolean::new(true).as_ref(),
            )?;
            decoder.hardware = hardware(
                decoder.session.as_ref(),
                kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
            )?;
        }
        Ok(decoder)
    }

    fn decode(&self, avcc: &[u8]) -> DecodedFrame {
        let mut block = ptr::null_mut();
        status(
            // SAFETY: A null memory block requests a CoreMedia-owned allocation of avcc.len() bytes;
            // all optional allocators/sources are null and the output pointer is writable.
            unsafe {
                CMBlockBuffer::create_with_memory_block(
                    None,
                    ptr::null_mut(),
                    avcc.len(),
                    None,
                    ptr::null(),
                    0,
                    avcc.len(),
                    kCMBlockBufferAssureMemoryNowFlag,
                    NonNull::from(&mut block),
                )
            },
            "CMBlockBufferCreateWithMemoryBlock",
        )?;
        let block = NonNull::new(block).ok_or_else(|| missing("decoder block buffer"))?;
        // SAFETY: Successful Create transfers the block buffer's +1 reference.
        let block = unsafe { CFRetained::from_raw(block) };
        status(
            // SAFETY: The nonempty slice has avcc.len() readable bytes, copied into the block's allocation.
            unsafe {
                CMBlockBuffer::replace_data_bytes(
                    NonNull::from(&avcc[0]).cast(),
                    &block,
                    0,
                    avcc.len(),
                )
            },
            "CMBlockBufferReplaceDataBytes",
        )?;
        let mut sample = ptr::null_mut();
        let length = avcc.len();
        status(
            // SAFETY: Live owned block and format, one compressed sample with length equal to the block.
            // No timing entries are needed for synchronous decoding without temporal processing.
            unsafe {
                CMSampleBuffer::create_ready(
                    None,
                    Some(&block),
                    Some(&self.format),
                    1,
                    0,
                    ptr::null(),
                    1,
                    &length,
                    NonNull::from(&mut sample),
                )
            },
            "CMSampleBufferCreateReady",
        )?;
        let sample = NonNull::new(sample).ok_or_else(|| missing("decoder sample"))?;
        // SAFETY: Successful Create transfers the sample's +1 reference.
        let sample = unsafe { CFRetained::from_raw(sample) };
        status(
            // SAFETY: Live session/sample, no borrowed refcon. Both async and temporal-processing flags
            // are clear, so VT guarantees the output callback completes before this call returns.
            unsafe {
                self.session.decode_frame(
                    &sample,
                    VTDecodeFrameFlags::empty(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            "VTDecompressionSessionDecodeFrame",
        )?;
        let frame = self
            .output
            .try_recv()
            .map_err(|_| missing("decoded frame"))??;
        if self.output.try_recv().is_ok() {
            return Err(CodecError::Failed(
                "multiple decoded frames for one access unit".into(),
            ));
        }
        Ok(frame)
    }
}

impl VideoDecoder for Decoder {
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<PixelSize, CodecError> {
        let result = (|| {
            let nals = annex_b_nals(data)?;
            let sps = nals.iter().find(|nal| nal[0] & 31 == 7).copied();
            let pps = nals.iter().find(|nal| nal[0] & 31 == 8).copied();
            let idr = nals.iter().any(|nal| nal[0] & 31 == 5);
            if let Some(sps) = sps
                && self.sps != sps
            {
                self.sps = sps.to_vec();
                self.session = None;
                self.last_reference = None;
            }
            if let Some(pps) = pps
                && self.pps != pps
            {
                self.pps = pps.to_vec();
                self.session = None;
                self.last_reference = None;
            }
            if self.sps.is_empty() || self.pps.is_empty() {
                return Err(CodecError::Failed("slice before SPS/PPS".into()));
            }
            if !idr && self.last_reference.is_none() {
                return Err(CodecError::Failed("decoder needs an IDR".into()));
            }
            if self.session.is_none() {
                self.session = Some(DecodeSession::new(&self.sps, &self.pps)?);
            }
            let session = self
                .session
                .as_ref()
                .ok_or_else(|| missing("decoder session"))?;
            let mut picture = None;
            let mut avcc = Vec::new();
            for nal in nals {
                if matches!(nal[0] & 31, 1 | 5) {
                    let mut bits = Bits::new(&nal[1..]);
                    bits.ue()?; // first_mb_in_slice
                    let slice_type = bits.ue()?;
                    if slice_type > 9 || slice_type % 5 == 1 {
                        return Err(CodecError::BadInput("B-frames are not supported"));
                    }
                    bits.ue()?; // pic_parameter_set_id (validated by CoreMedia/VT)
                    let frame_num = bits.read(session.frame_num_bits)?;
                    let reference = nal[0] & 0x60 != 0;
                    let slice = (frame_num, reference, nal[0] & 31 == 5);
                    if picture.is_some_and(|previous| previous != slice) {
                        return Err(CodecError::BadInput("multiple pictures in one access unit"));
                    }
                    picture = Some(slice);
                    let length = u32::try_from(nal.len())
                        .map_err(|_| CodecError::BadInput("NAL too large"))?;
                    avcc.extend_from_slice(&length.to_be_bytes());
                    avcc.extend_from_slice(nal);
                }
            }
            if avcc.is_empty() {
                return Err(CodecError::BadInput("access unit contains no slice"));
            }
            let (frame_num, reference, _) =
                picture.ok_or(CodecError::BadInput("access unit contains no slice"))?;
            // VT may conceal missing reference pictures instead of returning ReferenceMissingErr.
            // ponytail: reject reference-number gaps until an IDR; model the full decoded-picture
            // buffer only if streams intentionally allowing reference-number gaps are required.
            if !idr
                && self
                    .last_reference
                    .is_some_and(|last| frame_num != (last + 1) % (1 << session.frame_num_bits))
            {
                return Err(CodecError::Failed(
                    "missing H.264 reference frame; decoder needs an IDR".into(),
                ));
            }
            let frame = session.decode(&avcc)?;
            if reference {
                self.last_reference = Some(frame_num);
            }
            Ok(frame)
        })();
        match result {
            Ok((size, pixels)) => {
                *out = pixels;
                Ok(size)
            }
            Err(error) => {
                self.last_reference = None;
                self.session = None;
                Err(error)
            }
        }
    }

    fn name(&self) -> &str {
        match &self.session {
            Some(session) if session.hardware => "VideoToolbox (hardware)",
            Some(_) => "VideoToolbox (software)",
            None => "VideoToolbox",
        }
    }
}

// Only the SPS prefix and slice prefix are needed to guard reference continuity. CoreMedia and
// VideoToolbox validate the remaining H.264 syntax. Remove emulation-prevention bytes first.
struct Bits {
    bytes: Vec<u8>,
    position: usize,
}

impl Bits {
    fn new(ebsp: &[u8]) -> Self {
        let mut bytes = Vec::with_capacity(ebsp.len());
        let mut zeros = 0;
        for &byte in ebsp {
            if zeros == 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            bytes.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        Self { bytes, position: 0 }
    }

    fn read(&mut self, count: u32) -> Result<u32, CodecError> {
        let mut value = 0;
        for _ in 0..count {
            let byte = self
                .bytes
                .get(self.position / 8)
                .ok_or(CodecError::BadInput("truncated H.264 header"))?;
            value = (value << 1) | u32::from((byte >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(value)
    }

    fn ue(&mut self) -> Result<u32, CodecError> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(CodecError::BadInput("invalid H.264 Exp-Golomb value"));
            }
        }
        Ok((1u32 << zeros) - 1 + self.read(zeros)?)
    }
}

fn frame_num_bits(sps: &[u8]) -> Result<u32, CodecError> {
    let mut bits = Bits::new(&sps[1..]);
    let profile = bits.read(8)?;
    bits.read(16)?; // constraint flags and level_idc
    bits.ue()?; // seq_parameter_set_id
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        if bits.ue()? != 1 || bits.ue()? != 0 || bits.ue()? != 0 {
            return Err(CodecError::BadInput("expected 8-bit 4:2:0 H.264"));
        }
        bits.read(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.read(1)? != 0 {
            // seq_scaling_matrix_present_flag
            for index in 0..8 {
                if bits.read(1)? != 0 {
                    let mut last = 8i64;
                    let mut next = 8i64;
                    for _ in 0..if index < 6 { 16 } else { 64 } {
                        if next != 0 {
                            let value = i64::from(bits.ue()?);
                            let delta = if value & 1 == 0 {
                                -(value / 2)
                            } else {
                                (value + 1) / 2
                            };
                            next = (last + delta).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    let minus_four = bits.ue()?;
    if minus_four > 12 {
        return Err(CodecError::BadInput("invalid H.264 frame_num width"));
    }
    Ok(minus_four + 4)
}

fn annex_b_nals(data: &[u8]) -> Result<Vec<&[u8]>, CodecError> {
    let starts: Vec<_> = data
        .windows(3)
        .enumerate()
        .filter_map(|(i, bytes)| (bytes == [0, 0, 1]).then_some(i))
        .collect();
    let first = *starts
        .first()
        .ok_or(CodecError::BadInput("missing Annex B start code"))?;
    if data[..first].iter().any(|&byte| byte != 0) {
        return Err(CodecError::BadInput("bytes before Annex B start code"));
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (index, start) in starts.iter().enumerate() {
        let mut end = starts.get(index + 1).copied().unwrap_or(data.len());
        while end > start + 3 && data[end - 1] == 0 {
            end -= 1;
        }
        let nal = &data[start + 3..end];
        if nal.is_empty() || nal[0] & 0x80 != 0 {
            return Err(CodecError::BadInput("invalid Annex B NAL"));
        }
        nals.push(nal);
    }
    Ok(nals)
}

unsafe extern "C-unwind" fn decoded_callback(
    context: *mut c_void,
    _source: *mut c_void,
    code: i32,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _timestamp: CMTime,
    _duration: CMTime,
) {
    // SAFETY: Context is the live boxed sender. VT supplies a nullable image valid during this
    // callback; copy_image borrows it only while locked and returns owned Rust bytes.
    let (sender, image) = unsafe {
        (
            &*context.cast::<mpsc::Sender<DecodedFrame>>(),
            image.as_ref(),
        )
    };
    let result = status(code, "decompression callback").and_then(|()| {
        if flags.contains(VTDecodeInfoFlags::FrameDropped) {
            return Err(missing("decoded frame"));
        }
        image
            .ok_or_else(|| missing("decoded image"))
            .and_then(copy_image)
    });
    let _ = sender.send(result);
}

fn copy_image(image: &CVPixelBuffer) -> DecodedFrame {
    if CVPixelBufferGetPixelFormatType(image) != kCVPixelFormatType_32BGRA {
        return Err(CodecError::Failed("decoder output is not BGRA".into()));
    }
    let width = CVPixelBufferGetWidth(image);
    let height = CVPixelBufferGetHeight(image);
    let size = PixelSize::new(
        u32::try_from(width).map_err(|_| missing("valid width"))?,
        u32::try_from(height).map_err(|_| missing("valid height"))?,
    );
    if coded_size(size)? != size {
        return Err(CodecError::Failed(
            "decoder output has odd dimensions".into(),
        ));
    }
    status(
        // SAFETY: A live nonplanar BGRA buffer, locked/unlocked with matching read-only flags.
        unsafe { CVPixelBufferLockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) },
        "CVPixelBufferLockBaseAddress",
    )?;
    let copied = (|| {
        let stride = CVPixelBufferGetBytesPerRow(image);
        let len = CVPixelBufferGetDataSize(image);
        let base = CVPixelBufferGetBaseAddress(image).cast::<u8>();
        if base.is_null()
            || len > isize::MAX as usize
            || stride < width * 4
            || stride.checked_mul(height).is_none_or(|n| n > len)
        {
            return Err(missing("valid decoded storage"));
        }
        // SAFETY: CoreVideo supplies len live bytes under the read-only lock; stride/extent were
        // checked. The slice is borrowed only while locked, and no native pointer escapes.
        let source = unsafe { std::slice::from_raw_parts(base, len) };
        let mut out = Vec::with_capacity(width * 4 * height);
        for row in source.chunks_exact(stride).take(height) {
            out.extend_from_slice(&row[..width * 4]);
        }
        Ok((size, out))
    })();
    // SAFETY: Balances the successful lock on all copy paths; no source slice remains borrowed.
    let unlocked = status(
        unsafe { CVPixelBufferUnlockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) },
        "CVPixelBufferUnlockBaseAddress",
    );
    unlocked?;
    copied
}
