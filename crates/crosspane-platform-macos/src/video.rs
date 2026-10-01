//! Synchronous, low-latency H.264 through the public VideoToolbox APIs.

use std::any::Any;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Arc, mpsc};

use crate::frame_capture::{CaptureInput, SckImage};
use crosspane_media::codec::{
    CodecError, EncodedVideo, NativeInput, NativeInputPool, VideoCodecs, VideoDecoder, VideoEncoder,
};
#[cfg(feature = "gpu")]
use crosspane_media::picture::NativePicture;
use crosspane_media::picture::{Decoded, Nv12, YuvColour, YuvMatrix};
use crosspane_platform::NativeImage;
use crosspane_types::geom::PixelSize;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    CMBlockBuffer, CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescription,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, CMVideoFormatDescriptionGetDimensions,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMBlockBufferAssureMemoryNowFlag,
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVAttachmentMode, CVImageBuffer, CVPixelBuffer, CVPixelBufferCreate,
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetDataSize, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferColorPrimariesKey,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferTransferFunction_sRGB,
    kCVImageBufferTransferFunctionKey, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVImageBufferYCbCrMatrixKey, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferPoolAllocationThresholdKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTDecodeFrameFlags, VTDecodeInfoFlags,
    VTDecompressionOutputCallbackRecord, VTDecompressionSession, VTEncodeInfoFlags,
    VTIsHardwareDecodeSupported, VTPixelTransferSession, VTSessionCopyProperty,
    VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ColorPrimaries,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTDecompressionPropertyKey_RealTime,
    kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTPixelTransferPropertyKey_DestinationYCbCrMatrix,
    kVTProfileLevel_H264_High_AutoLevel,
    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const MAX_DECODE_DIMENSION: u32 = 8192;
type EncodedFrame = Result<(EncodedVideo, Vec<u8>), CodecError>;
type DecodedFrame = Result<CFRetained<CVPixelBuffer>, CodecError>;

#[derive(Debug, Default)]
pub struct VtCodecs;

impl VtCodecs {
    pub fn new() -> Self {
        Self
    }

    pub fn nv12_decoder(&self) -> Result<Decoder, CodecError> {
        // SAFETY: Public capability probe, no session or callback involved.
        Decoder::new(unsafe { VTIsHardwareDecodeSupported(kCMVideoCodecType_H264) })
    }

    /// Select the allocation fallback explicitly for comparing encoder input paths.
    #[doc(hidden)]
    pub fn encoder_with_fallback(
        &self,
        size: PixelSize,
        bitrate: u32,
        fps: u32,
        fallback: bool,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        if fps == 0 || fps > i32::MAX as u32 || bitrate == 0 {
            return Err(CodecError::BadInput(
                "bitrate and frame rate must be positive",
            ));
        }
        let mut encoder = Encoder::new(size, bitrate, fps)?;
        encoder.fallback = fallback;
        if !fallback {
            let session = encoder
                .session
                .as_deref()
                .ok_or_else(|| missing("compression session"))?;
            // SAFETY: Live prepared session; the public getter retains its pool.
            let pool =
                unsafe { session.pixel_buffer_pool() }.ok_or_else(|| missing("input pool"))?;
            let _ = pool_buffer(&pool)?;
        }
        Ok(Box::new(encoder))
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
        // SAFETY: Public capability probe with a documented H.264 codec identifier, no session needed.
        let supported = unsafe { VTIsHardwareDecodeSupported(kCMVideoCodecType_H264) };
        Ok(Box::new(Decoder::new(supported)?))
    }
}

fn status(code: i32, operation: &str) -> Result<(), CodecError> {
    if code == 0 {
        Ok(())
    } else {
        Err(CodecError::Failed(format!("{operation}: OSStatus {code}")))
    }
}

fn create_status(code: i32, operation: &str) -> Result<(), CodecError> {
    if code == 0 {
        Ok(())
    } else {
        Err(CodecError::Unavailable(format!(
            "{operation}: OSStatus {code}"
        )))
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
    session: Option<CFRetained<VTCompressionSession>>,
    // Keep the native session first. The raw Box is explicitly reclaimed after teardown in Drop,
    // so callback lifetime does not depend on automatic field drop order.
    callback: *mut mpsc::Sender<EncodedFrame>,
    output: mpsc::Receiver<EncodedFrame>,
    size: PixelSize,
    bitrate: u32,
    applied_bitrate: u32,
    fps: u32,
    frame: i64,
    first: bool,
    fallback: bool,
    fallback_logged: bool,
    transfer: &'static CFString,
    nv12: bool,
    pool: Option<Arc<InputPool>>,
}

// SAFETY: VT sessions have no thread affinity. &mut self serializes all use; callbacks access
// only the stable, thread-safe sender, never the session or other Encoder fields.
unsafe impl Send for Encoder {}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.reset();
        // SAFETY: This pointer came from Box::into_raw and is reclaimed exactly once, after all
        // frames were drained and the native session invalidated. No callback can still borrow it.
        unsafe { drop(Box::from_raw(self.callback)) };
    }
}

impl Encoder {
    fn reset(&mut self) {
        if let Some(session) = self.session.take() {
            // SAFETY: Live owned session, with its callback allocation still live. Invalid time
            // drains all pending frames before invalidation, including after a failed encode.
            unsafe {
                let _ = session.complete_frames(kCMTimeInvalid);
                session.invalidate();
            }
        }
        for _ in self.output.try_iter() {}
        self.first = true;
        self.frame = 0;
        self.applied_bitrate = 0;
    }

    fn new(size: PixelSize, bitrate: u32, fps: u32) -> Result<Self, CodecError> {
        Self::new_format(size, bitrate, fps, false)
    }

    fn new_format(size: PixelSize, bitrate: u32, fps: u32, nv12: bool) -> Result<Self, CodecError> {
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
        let attributes = if nv12 {
            input_attributes(coded, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        } else {
            bgra_attributes(coded)
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
                Some(attributes.as_ref()),
                None,
                Some(encoded_callback),
                ptr::from_mut(&mut *callback).cast(),
                NonNull::from(&mut session),
            )
        };
        create_status(code, "VTCompressionSessionCreate")?;
        let session = NonNull::new(session).ok_or_else(|| missing("compression session"))?;
        // SAFETY: Successful Create transfers the session's +1 reference.
        let session = unsafe { CFRetained::from_raw(session) };
        // SAFETY: Immutable exported colour constant.
        let mut encoder = Self {
            session: Some(session),
            callback: Box::into_raw(callback),
            output,
            size,
            bitrate,
            applied_bitrate: bitrate,
            fps,
            frame: 0,
            first: true,
            fallback: false,
            fallback_logged: false,
            // SAFETY: Immutable exported colour constant.
            transfer: unsafe { kCVImageBufferTransferFunction_sRGB },
            nv12,
            pool: None,
        };
        let session = encoder
            .session
            .as_deref()
            .ok_or_else(|| missing("compression session"))?;
        // SAFETY: Immutable public property keys. Each value has the property's documented type.
        unsafe {
            set(
                session.as_ref(),
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2.as_ref(),
            )?;
            set(
                session.as_ref(),
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_ITU_R_709_2.as_ref(),
            )?;
            if set(
                session.as_ref(),
                kVTCompressionPropertyKey_TransferFunction,
                encoder.transfer.as_ref(),
            )
            .is_err()
            {
                encoder.transfer = kCVImageBufferTransferFunction_ITU_R_709_2;
                set(
                    session.as_ref(),
                    kVTCompressionPropertyKey_TransferFunction,
                    encoder.transfer.as_ref(),
                )?;
            }
            set(
                session.as_ref(),
                kVTCompressionPropertyKey_RealTime,
                CFBoolean::new(true).as_ref(),
            )?;
            set(
                session.as_ref(),
                kVTCompressionPropertyKey_AllowFrameReordering,
                CFBoolean::new(false).as_ref(),
            )?;
            set(
                session.as_ref(),
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_High_AutoLevel.as_ref(),
            )?;
            set(
                session.as_ref(),
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                CFNumber::new_i32(i32::MAX).as_ref(),
            )?;
            set(
                session.as_ref(),
                kVTCompressionPropertyKey_ExpectedFrameRate,
                CFNumber::new_i32(fps as i32).as_ref(),
            )?;
            set(
                session.as_ref(),
                kVTCompressionPropertyKey_AverageBitRate,
                CFNumber::new_i64(i64::from(bitrate)).as_ref(),
            )?;
            status(
                session.prepare_to_encode_frames(),
                "VTCompressionSessionPrepareToEncodeFrames",
            )?;
        }
        tracing::debug!(transfer = %encoder.transfer, "VideoToolbox encoder colour transfer");
        Ok(encoder)
    }

    fn ensure_format(&mut self, size: PixelSize, nv12: bool) -> Result<(), CodecError> {
        if self.size != size || self.nv12 != nv12 || self.session.is_none() {
            let fallback = self.fallback;
            let fallback_logged = self.fallback_logged;
            self.reset();
            *self = Self::new_format(size, self.bitrate, self.fps, nv12)?;
            self.fallback = fallback;
            self.fallback_logged = fallback_logged;
        }
        Ok(())
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
        self.ensure_format(size, false)?;
        let session = self
            .session
            .as_deref()
            .ok_or_else(|| missing("compression session"))?;
        let pooled = if self.fallback {
            None
        } else {
            // SAFETY: Live prepared compression session; getter retains the pool.
            unsafe { session.pixel_buffer_pool() }.and_then(|pool| pool_buffer(&pool).ok())
        };
        let image = if let Some(image) = pooled {
            fill_input_buffer(image, pixels, stride as usize, size, coded)?
        } else {
            if !self.fallback && !self.fallback_logged {
                tracing::warn!("VideoToolbox input pool unavailable; using allocated BGRA buffers");
                self.fallback_logged = true;
            }
            input_buffer(pixels, stride as usize, size, coded)?
        };
        self.submit(&image, force_key, out)
    }

    fn input_pool(
        &mut self,
        size: PixelSize,
    ) -> Result<Option<Arc<dyn NativeInputPool>>, CodecError> {
        self.ensure_format(size, true)?;
        if self.pool.is_none() {
            let session = self
                .session
                .as_deref()
                .ok_or_else(|| missing("compression session"))?;
            // SAFETY: Live prepared session; the getter retains its pool.
            let pool =
                unsafe { session.pixel_buffer_pool() }.ok_or_else(|| missing("NV12 input pool"))?;
            self.pool = Some(Arc::new(InputPool {
                pool,
                size: coded_size(size)?,
            }));
        }
        Ok(self
            .pool
            .as_ref()
            .map(|pool| Arc::clone(pool) as Arc<dyn NativeInputPool>))
    }

    fn encode_native(
        &mut self,
        input: &dyn NativeInput,
        size: PixelSize,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedVideo, CodecError> {
        out.clear();
        let coded = coded_size(size)?;
        if let Some(input) = input.as_any().downcast_ref::<PoolInput>() {
            if input.size() != coded
                || self.size != size
                || !self.nv12
                || self.session.is_none()
                || !self
                    .pool
                    .as_ref()
                    .is_some_and(|pool| ptr::eq(&*pool.pool, &*input.pool))
            {
                return Err(CodecError::BadInput("foreign or stale VT pool input"));
            }
            return self.submit(&input.buffer, force_key, out);
        }
        let input = input
            .as_any()
            .downcast_ref::<CaptureInput>()
            .and_then(|input| input.0.as_any().downcast_ref::<SckImage>())
            .ok_or(CodecError::BadInput("foreign native input"))?;
        if input.size() != size {
            return Err(CodecError::BadInput("capture size mismatch"));
        }
        self.ensure_format(size, false)?;
        // VT encodes a whole buffer. A capture that is exactly its buffer (the twin display, which
        // SCK crops with sourceRect) goes in as is; a region of a larger buffer (window capture)
        // or an odd size (VT can't repeat the last column and row as `encode` pads) is copied,
        // and only that region. SCK's buffer is never modified.
        let full = PixelSize::new(
            CVPixelBufferGetWidth(&input.buffer) as u32,
            CVPixelBufferGetHeight(&input.buffer) as u32,
        );
        if coded != size || full != size || input.region.min.x != 0 || input.region.min.y != 0 {
            return self.copy_capture(input, size, force_key, out);
        }
        let _access = input.access.lock().unwrap_or_else(|e| e.into_inner());
        self.submit(&input.buffer, force_key, out)
    }

    fn set_bitrate(&mut self, bits_per_second: u32) {
        if bits_per_second != 0 {
            self.bitrate = bits_per_second;
        }
    }
    fn name(&self) -> &str {
        "VideoToolbox (hardware required)"
    }
}

impl Encoder {
    fn copy_capture(
        &mut self,
        input: &SckImage,
        size: PixelSize,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedVideo, CodecError> {
        let coded = coded_size(size)?;
        let mut image = None;
        input
            .read(&mut |pixels, stride| {
                image = Some(input_buffer(pixels, stride as usize, size, coded));
            })
            .map_err(|_| missing("capture mapping for crop/padding"))?;
        let image = image.ok_or_else(|| missing("mapped capture"))??;
        self.submit(&image, force_key, out)
    }

    fn submit(
        &mut self,
        image: &CVPixelBuffer,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedVideo, CodecError> {
        let session = self
            .session
            .as_deref()
            .ok_or_else(|| missing("compression session"))?;
        // SAFETY: Public colour keys and live buffer; attachments retain their values.
        unsafe {
            for (key, value) in [
                (
                    kCVImageBufferYCbCrMatrixKey,
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                ),
                (
                    kCVImageBufferColorPrimariesKey,
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                ),
                (kCVImageBufferTransferFunctionKey, self.transfer),
            ] {
                image.set_attachment(key, value.as_ref(), CVAttachmentMode::ShouldPropagate);
            }
        }
        let force = force_key || self.first;
        // SAFETY: Immutable public property keys, with boolean/numeric values of the required types.
        let properties = unsafe {
            if self.applied_bitrate != self.bitrate {
                set(
                    session.as_ref(),
                    kVTCompressionPropertyKey_AverageBitRate,
                    CFNumber::new_i64(i64::from(self.bitrate)).as_ref(),
                )?;
                self.applied_bitrate = self.bitrate;
            }
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
                session.encode_frame(
                    image,
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
                    session.complete_frames(timestamp),
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
                out.try_reserve_exact(frame.1.len())
                    .map_err(|_| CodecError::Failed("encoded output allocation failed".into()))?;
                Ok(frame)
            }
        });
        match result {
            Ok((encoded, bytes)) => {
                out.extend_from_slice(&bytes);
                self.first = false;
                Ok(encoded)
            }
            Err(error) => {
                // A failed/dropped frame must not leave callback output or references for the next
                // call. Tear down now; the next encode creates a fresh session and starts on IDR.
                // If recreation fails, session stays None, so a later call retries cleanly.
                self.reset();
                Err(error)
            }
        }
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
    fill_input_buffer(image, pixels, stride, size, coded)
}

fn bgra_attributes(size: PixelSize) -> CFRetained<CFDictionary<CFString, CFType>> {
    input_attributes(size, kCVPixelFormatType_32BGRA)
}

fn input_attributes(size: PixelSize, format: u32) -> CFRetained<CFDictionary<CFString, CFType>> {
    let surface = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
    // SAFETY: Immutable public keys with documented CFNumber/dictionary values.
    unsafe {
        CFDictionary::from_slices(
            &[
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferWidthKey,
                kCVPixelBufferHeightKey,
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            ],
            &[
                CFNumber::new_i64(i64::from(format)).as_ref(),
                CFNumber::new_i64(i64::from(size.width)).as_ref(),
                CFNumber::new_i64(i64::from(size.height)).as_ref(),
                surface.as_ref(),
                CFBoolean::new(true).as_ref(),
            ],
        )
    }
}

#[derive(Debug)]
pub(crate) struct InputPool {
    pool: CFRetained<CVPixelBufferPool>,
    size: PixelSize,
}

// SAFETY: CoreVideo pools support concurrent allocation; CF retain/release is thread-safe.
unsafe impl Send for InputPool {}
// SAFETY: The immutable wrapper only calls the pool's thread-safe allocator.
unsafe impl Sync for InputPool {}

#[derive(Debug)]
pub(crate) struct PoolInput {
    pub(crate) buffer: CFRetained<CVPixelBuffer>,
    pool: CFRetained<CVPixelBufferPool>,
    size: PixelSize,
}

// SAFETY: Ownership keeps storage alive. Writers must finish before encode_native as required
// by NativeInput; this wrapper does not expose a safe mutable mapping.
unsafe impl Send for PoolInput {}
// SAFETY: Shared references only expose immutable metadata; mutation requires external GPU/FFI
// synchronization under NativeInput's completed-writer contract.
unsafe impl Sync for PoolInput {}

impl NativeInput for PoolInput {
    fn size(&self) -> PixelSize {
        self.size
    }
    fn colour(&self) -> YuvColour {
        YuvColour {
            matrix: YuvMatrix::Bt709,
            full_range: false,
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// Retain pool identity in each input, so inputs surviving a session change are rejected.
impl NativeInputPool for InputPool {
    fn size(&self) -> PixelSize {
        self.size
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn acquire(&self) -> Result<Arc<dyn NativeInput>, CodecError> {
        let mut image = ptr::null_mut();
        // SAFETY: Public threshold key and numeric CF value. CoreVideo returns immediately with
        // kCVReturnWouldExceedAllocationThreshold when all eight buffers are retained/in flight.
        let attributes = unsafe {
            CFDictionary::from_slices(
                &[kCVPixelBufferPoolAllocationThresholdKey],
                &[&*CFNumber::new_i32(8)],
            )
        };
        // SAFETY: Live pool, correctly typed auxiliary attributes and writable output pointer.
        status(
            // SAFETY: Live pool, typed auxiliary attributes and writable out pointer.
            unsafe {
                CVPixelBufferPool::create_pixel_buffer_with_aux_attributes(
                    None,
                    &self.pool,
                    Some(attributes.as_ref()),
                    NonNull::from(&mut image),
                )
            },
            "CVPixelBufferPoolCreatePixelBufferWithAuxAttributes",
        )?;
        let image = NonNull::new(image).ok_or_else(|| missing("NV12 pool buffer"))?;
        // SAFETY: Successful Create transfers a +1 reference.
        let buffer = unsafe { CFRetained::from_raw(image) };
        if CVPixelBufferGetPixelFormatType(&buffer)
            != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            || CVPixelBufferGetWidth(&buffer) != self.size.width as usize
            || CVPixelBufferGetHeight(&buffer) != self.size.height as usize
        {
            return Err(missing("matching NV12 pool buffer"));
        }
        Ok(Arc::new(PoolInput {
            buffer,
            pool: self.pool.clone(),
            size: self.size,
        }))
    }
}

fn pool_buffer(pool: &CVPixelBufferPool) -> Result<CFRetained<CVPixelBuffer>, CodecError> {
    let mut image = ptr::null_mut();
    status(
        // SAFETY: Live pool and writable output; successful Create transfers a +1 reference.
        unsafe { CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::from(&mut image)) },
        "CVPixelBufferPoolCreatePixelBuffer",
    )?;
    let image = NonNull::new(image).ok_or_else(|| missing("pooled pixel buffer"))?;
    // SAFETY: Successful Create transfers its +1 reference.
    Ok(unsafe { CFRetained::from_raw(image) })
}

fn fill_input_buffer(
    image: CFRetained<CVPixelBuffer>,
    pixels: &[u8],
    stride: usize,
    size: PixelSize,
    coded: PixelSize,
) -> Result<CFRetained<CVPixelBuffer>, CodecError> {
    if CVPixelBufferGetPixelFormatType(&image) != kCVPixelFormatType_32BGRA
        || CVPixelBufferGetWidth(&image) != coded.width as usize
        || CVPixelBufferGetHeight(&image) != coded.height as usize
    {
        return Err(missing("matching BGRA input buffer"));
    }
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
    let mut sync = true; // Absence of NotSync is CoreMedia's sync-sample convention.
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
            sync = !value
                .downcast_ref::<CFBoolean>()
                .ok_or_else(|| missing("NotSync boolean"))?
                .as_bool();
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
    let nals = avcc_nals(&avcc)?;
    let key = nals.iter().any(|nal| nal[0] & 31 == 5);
    if key && !sync {
        return Err(CodecError::Failed(
            "IDR sample has a NotSync attachment".into(),
        ));
    }
    let mut out = Vec::new();
    if key {
        let format = format.ok_or_else(|| missing("H.264 format description"))?;
        for index in 0..2 {
            let mut bytes = ptr::null();
            let mut len = 0;
            let mut header_len = 0;
            let mut count = 0;
            status(
                // SAFETY: Live H.264 format; writable outputs. The returned parameter bytes remain
                // owned by the retained format and are copied before releasing it.
                unsafe {
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        &format,
                        index,
                        &mut bytes,
                        &mut len,
                        &mut count,
                        &mut header_len,
                    )
                },
                "CMVideoFormatDescriptionGetH264ParameterSetAtIndex",
            )?;
            if bytes.is_null()
                || len == 0
                || len > isize::MAX as usize
                || header_len != 4
                || count != 2
            {
                return Err(CodecError::Failed(
                    "invalid H.264 parameter set or NAL length size".into(),
                ));
            }
            // SAFETY: The retained format supplies len live bytes, checked above.
            let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
            if bytes[0] & 0x80 != 0 || bytes[0] & 31 != 7 + index as u8 {
                return Err(CodecError::Failed(
                    "unexpected H.264 parameter-set NAL type".into(),
                ));
            }
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(bytes);
        }
    }
    for nal in nals {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    Ok((EncodedVideo { key }, out))
}

fn avcc_nals(avcc: &[u8]) -> Result<Vec<&[u8]>, CodecError> {
    let mut nals = Vec::new();
    let mut remaining = avcc;
    while !remaining.is_empty() {
        let length = remaining
            .get(..4)
            .ok_or(CodecError::BadInput("truncated AVCC length"))?;
        let length = u32::from_be_bytes([length[0], length[1], length[2], length[3]]) as usize;
        remaining = &remaining[4..];
        if length == 0 || length > remaining.len() {
            return Err(CodecError::BadInput("invalid AVCC NAL length"));
        }
        if remaining[0] & 0x80 != 0 {
            return Err(CodecError::BadInput("invalid AVCC NAL"));
        }
        nals.push(&remaining[..length]);
        remaining = &remaining[length..];
    }
    if avcc.is_empty() {
        return Err(missing("H.264 NAL units"));
    }
    Ok(nals)
}

pub struct Decoder {
    sps: Vec<u8>,
    pps: Vec<u8>,
    session: Option<DecodeSession>,
    last_reference: Option<u32>,
}

#[cfg(feature = "gpu")]
#[derive(Debug)]
pub(crate) struct VtPicture {
    pub(crate) image: CFRetained<CVPixelBuffer>,
    size: PixelSize,
    colour: YuvColour,
}

// SAFETY: CoreVideo buffers are reference counted thread-safely; this immutable picture
// never writes to the buffer, and VT cannot recycle it while its retain is held.
#[cfg(feature = "gpu")]
unsafe impl Send for VtPicture {}
// SAFETY: The same immutable, thread-safe CoreVideo retain contract permits shared reads.
#[cfg(feature = "gpu")]
unsafe impl Sync for VtPicture {}

#[cfg(feature = "gpu")]
impl NativePicture for VtPicture {
    fn size(&self) -> PixelSize {
        self.size
    }
    fn colour(&self) -> YuvColour {
        self.colour
    }
    fn to_nv12(&self, out: &mut Nv12) -> Result<(), CodecError> {
        copy_nv12(&self.image, out)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(feature = "gpu")]
impl VtPicture {
    pub(crate) fn retained(&self) -> Self {
        Self {
            image: self.image.clone(),
            size: self.size,
            colour: self.colour,
        }
    }
}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoder")
            .field("last_reference", &self.last_reference)
            .finish_non_exhaustive()
    }
}

impl Decoder {
    fn new(hardware_supported: bool) -> Result<Self, CodecError> {
        if !hardware_supported {
            return Err(CodecError::Unavailable(
                "hardware H.264 decode is not supported".into(),
            ));
        }
        Ok(Self {
            sps: Vec::new(),
            pps: Vec::new(),
            session: None,
            last_reference: None,
        })
    }
}

struct DecodeSession {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMVideoFormatDescription>,
    // Raw Box ownership makes callback teardown independent of this field's drop order.
    callback: *mut mpsc::Sender<DecodedFrame>,
    output: mpsc::Receiver<DecodedFrame>,
    hardware: Option<bool>,
    frame_num_bits: u32,
    nv12: bool,
}

// SAFETY: VT and CoreMedia have no thread affinity. &mut Decoder serializes use; callbacks access
// only the stable sender. The format is immutable; returned buffers are retained and read-only.
unsafe impl Send for DecodeSession {}

impl Drop for DecodeSession {
    fn drop(&mut self) {
        // SAFETY: The session and its callback allocation remain live throughout draining and
        // invalidation. Reclaim the Box exactly once afterwards, before any fields are dropped.
        unsafe {
            let _ = self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
            drop(Box::from_raw(self.callback));
        }
    }
}

impl DecodeSession {
    #[cfg(test)]
    fn new(sps: &[u8], pps: &[u8]) -> Result<Self, CodecError> {
        Self::with_output(sps, pps, false)
    }

    fn with_output(sps: &[u8], pps: &[u8], nv12: bool) -> Result<Self, CodecError> {
        let frame_num_bits = frame_num_bits(sps)?;
        if pps.is_empty() {
            return Err(CodecError::BadInput("missing PPS"));
        }
        let mut pointers = [
            NonNull::new(sps.as_ptr().cast_mut()).ok_or(CodecError::BadInput("missing SPS"))?,
            NonNull::new(pps.as_ptr().cast_mut()).ok_or(CodecError::BadInput("missing PPS"))?,
        ];
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
        // SAFETY: A live H.264 format description; this getter only reads its encoded dimensions.
        let dimensions = unsafe { CMVideoFormatDescriptionGetDimensions(&format) };
        decode_size(dimensions.width, dimensions.height)?;
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
                {
                    let formats: CFRetained<CFType> = if nv12 {
                        let formats = CFArray::<CFType>::from_objects(&[
                            CFNumber::new_i64(i64::from(
                                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                            ))
                            .as_ref(),
                            CFNumber::new_i64(i64::from(
                                kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
                            ))
                            .as_ref(),
                        ]);
                        // SAFETY: Every CFArray is a CFType; erasing its element type
                        // preserves the retained object and ownership.
                        CFRetained::cast_unchecked(formats)
                    } else {
                        CFNumber::new_i64(i64::from(kCVPixelFormatType_32BGRA)).into()
                    };
                    let surface = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
                    CFDictionary::<CFString, CFType>::from_slices(
                        &[
                            kCVPixelBufferPixelFormatTypeKey,
                            kCVPixelBufferIOSurfacePropertiesKey,
                            kCVPixelBufferMetalCompatibilityKey,
                        ],
                        &[
                            formats.as_ref(),
                            surface.as_ref(),
                            CFBoolean::new(true).as_ref(),
                        ],
                    )
                },
            )
        };
        let mut session = ptr::null_mut();
        // SAFETY: Retained H.264 format, documented destination attributes and correct callback.
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
        create_status(code, "VTDecompressionSessionCreate")?;
        let session = NonNull::new(session).ok_or_else(|| missing("decompression session"))?;
        // SAFETY: Successful Create transfers the session's +1 reference.
        let session = unsafe { CFRetained::from_raw(session) };
        let mut decoder = Self {
            session,
            format,
            callback: Box::into_raw(callback),
            output,
            hardware: None,
            frame_num_bits,
            nv12,
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
            )
            .ok();
        }
        Ok(decoder)
    }

    fn decode(&self, avcc: &[u8]) -> DecodedFrame {
        if avcc.is_empty() {
            return Err(CodecError::BadInput("missing AVCC data"));
        }
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
                    NonNull::new(avcc.as_ptr().cast_mut().cast())
                        .ok_or(CodecError::BadInput("missing AVCC data"))?,
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

impl Decoder {
    fn decode_image(&mut self, data: &[u8], nv12: bool) -> DecodedFrame {
        let result = (|| {
            let nals = annex_b_nals(data)?;
            let sps = nals.iter().find(|nal| nal[0] & 31 == 7).copied();
            let pps = nals.iter().find(|nal| nal[0] & 31 == 8).copied();
            let idr = nals.iter().any(|nal| nal[0] & 31 == 5);
            if idr
                && self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.nv12 != nv12)
            {
                self.session = None;
            }
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
                self.session = Some(DecodeSession::with_output(&self.sps, &self.pps, nv12)?);
            }
            let session = self
                .session
                .as_ref()
                .ok_or_else(|| missing("decoder session"))?;
            let mut picture = None;
            let mut avcc = Vec::new();
            for nal in nals {
                if matches!(nal[0] & 31, 1 | 5) {
                    // A slice prefix holds the three Exp-Golomb values and up to 16 frame_num
                    // bits. Bound the copy even when the untrusted slice payload is very large.
                    let mut bits = Bits::new(&nal[1..nal.len().min(33)]);
                    let first_mb = bits.ue()?;
                    let slice_type = bits.ue()?;
                    if slice_type > 9 || slice_type % 5 == 1 {
                        return Err(CodecError::BadInput("B-frames are not supported"));
                    }
                    bits.ue()?; // pic_parameter_set_id (validated by CoreMedia/VT)
                    let frame_num = bits.read(session.frame_num_bits)?;
                    let reference = nal[0] & 0x60 != 0;
                    let slice = (frame_num, reference, nal[0] & 31 == 5);
                    if picture.is_some_and(|previous| previous != slice || first_mb == 0) {
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
            Ok(image) => Ok(image),
            Err(error) => {
                self.last_reference = None;
                self.session = None;
                Err(error)
            }
        }
    }

    pub fn decode_nv12(&mut self, data: &[u8], out: &mut Nv12) -> Result<(), CodecError> {
        let image = self.decode_image(data, true)?;
        // Preserve references when callers change output between IDRs. At the next IDR,
        // decode_image can safely rebuild for the requested native output format.
        let result = if CVPixelBufferGetPixelFormatType(&image) == kCVPixelFormatType_32BGRA {
            transfer_image(&image, true).and_then(|image| copy_nv12(&image, out))
        } else {
            copy_nv12(&image, out)
        };
        if result.is_err() {
            self.session = None;
            self.last_reference = None;
        }
        result
    }
}

impl VideoDecoder for Decoder {
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<PixelSize, CodecError> {
        let image = self.decode_image(data, false)?;
        let result = if CVPixelBufferGetPixelFormatType(&image) == kCVPixelFormatType_32BGRA {
            copy_image(&image)
        } else {
            transfer_image(&image, false).and_then(|image| copy_image(&image))
        };
        match result {
            Ok((size, pixels)) => {
                out.try_reserve_exact(pixels.len().saturating_sub(out.len()))
                    .map_err(|_| CodecError::Failed("decoded output allocation failed".into()))?;
                out.clear();
                out.extend_from_slice(&pixels);
                Ok(size)
            }
            Err(error) => {
                self.session = None;
                self.last_reference = None;
                Err(error)
            }
        }
    }

    fn decode_nv12(&mut self, data: &[u8], out: &mut Nv12) -> Result<(), CodecError> {
        Decoder::decode_nv12(self, data, out)
    }

    fn decode_native(&mut self, data: &[u8], reuse: &mut Arc<Nv12>) -> Result<Decoded, CodecError> {
        let image = self.decode_image(data, true)?;
        #[cfg(feature = "gpu")]
        {
            let width = CVPixelBufferGetWidth(&image);
            let height = CVPixelBufferGetHeight(&image);
            let format = CVPixelBufferGetPixelFormatType(&image);
            if (format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                || format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)
                && width > 0
                && height > 0
                && width.is_multiple_of(2)
                && height.is_multiple_of(2)
                && CVPixelBufferGetPlaneCount(&image) == 2
                && objc2_core_video::CVPixelBufferGetIOSurface(Some(&image)).is_some()
            {
                let colour = image_colour(&image);
                return Ok(Decoded::Native(Arc::new(VtPicture {
                    image,
                    size: PixelSize::new(width as u32, height as u32),
                    colour,
                })));
            }
        }
        // Reuse the last picture only if nothing else holds it (no clone of a shown picture).
        if Arc::get_mut(reuse).is_none() {
            *reuse = Arc::default();
        }
        let out = Arc::get_mut(reuse).ok_or_else(|| missing("unshared NV12 picture"))?;
        let result = if CVPixelBufferGetPixelFormatType(&image) == kCVPixelFormatType_32BGRA {
            transfer_image(&image, true).and_then(|image| copy_nv12(&image, out))
        } else {
            copy_nv12(&image, out)
        };
        if result.is_err() {
            self.session = None;
            self.last_reference = None;
        }
        result?;
        Ok(Decoded::Nv12(Arc::clone(reuse)))
    }

    fn name(&self) -> &str {
        match &self.session {
            Some(session) => match session.hardware {
                Some(true) => "VideoToolbox (hardware)",
                Some(false) => "VideoToolbox (software)",
                None => "VideoToolbox (hardware required; status unknown)",
            },
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
    if sps
        .first()
        .is_none_or(|header| header & 0x80 != 0 || header & 31 != 7)
    {
        return Err(CodecError::BadInput("invalid SPS NAL"));
    }
    let mut bits = Bits::new(&sps[1..]);
    let profile = bits.read(8)?;
    if !matches!(
        profile,
        66 | 77 | 88 | 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        return Err(CodecError::BadInput("unsupported H.264 profile"));
    }
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
    // callback; retain it before returning, then copy its planes under a lock on the caller.
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
        image.ok_or_else(|| missing("decoded image")).map(|image| {
            // SAFETY: VT's callback image is live; retain before returning to VT.
            unsafe { CFRetained::retain(NonNull::from(image)) }
        })
    });
    let _ = sender.send(result);
}

fn transfer_image(
    image: &CVPixelBuffer,
    nv12: bool,
) -> Result<CFRetained<CVPixelBuffer>, CodecError> {
    let mut destination = ptr::null_mut();
    let size = decode_size(
        CVPixelBufferGetWidth(image) as i32,
        CVPixelBufferGetHeight(image) as i32,
    )?;
    let surface = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
    let format = if nv12 {
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
    } else {
        kCVPixelFormatType_32BGRA
    };
    // SAFETY: Public format and IOSurface keys with documented CF types.
    let attributes = unsafe {
        CFDictionary::<CFString, CFType>::from_slices(
            &[
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            ],
            &[
                CFNumber::new_i64(i64::from(format)).as_ref(),
                surface.as_ref(),
            ],
        )
    };
    let mut transfer = ptr::null_mut();
    // SAFETY: Live source, documented destination attributes, writable Create outputs.
    // Create references are owned below, transfer completes before invalidation/release.
    unsafe {
        status(
            CVPixelBufferCreate(
                None,
                size.width as usize,
                size.height as usize,
                format,
                Some(attributes.as_ref()),
                NonNull::from(&mut destination),
            ),
            "CVPixelBufferCreate alternate output",
        )?;
        let destination = NonNull::new(destination).ok_or_else(|| missing("alternate output"))?;
        let destination = CFRetained::from_raw(destination);
        status(
            VTPixelTransferSession::create(None, NonNull::from(&mut transfer)),
            "VTPixelTransferSessionCreate",
        )?;
        let transfer = NonNull::new(transfer).ok_or_else(|| missing("pixel transfer session"))?;
        let transfer = CFRetained::from_raw(transfer);
        if nv12 {
            set(
                transfer.as_ref(),
                kVTPixelTransferPropertyKey_DestinationYCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2.as_ref(),
            )?;
            destination.set_attachment(
                kCVImageBufferYCbCrMatrixKey,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2.as_ref(),
                CVAttachmentMode::ShouldPropagate,
            );
        }
        let result = status(
            transfer.transfer_image(image, &destination),
            "VTPixelTransferSessionTransferImage",
        );
        transfer.invalidate();
        result?;
        Ok(destination)
    }
}

fn image_colour(image: &CVPixelBuffer) -> YuvColour {
    // SAFETY: Immutable public matrix constants; attachment lookup retains its CF value.
    let matrix = unsafe {
        image
            .attachment(kCVImageBufferYCbCrMatrixKey, ptr::null_mut())
            .and_then(|value| {
                value.downcast_ref::<CFString>().map(|value| {
                    if value == kCVImageBufferYCbCrMatrix_ITU_R_601_4 {
                        YuvMatrix::Bt601
                    } else {
                        YuvMatrix::Bt709
                    }
                })
            })
            .unwrap_or(YuvMatrix::Bt709)
    };
    YuvColour {
        matrix,
        full_range: CVPixelBufferGetPixelFormatType(image)
            == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    }
}

fn copy_nv12(image: &CVPixelBuffer, out: &mut Nv12) -> Result<(), CodecError> {
    let format = CVPixelBufferGetPixelFormatType(image);
    if (format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        && format != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)
        || CVPixelBufferGetPlaneCount(image) != 2
    {
        return Err(CodecError::Failed("decoder output is not NV12".into()));
    }
    let size = decode_size(
        CVPixelBufferGetWidth(image) as i32,
        CVPixelBufferGetHeight(image) as i32,
    )?;
    if coded_size(size)? != size {
        return Err(CodecError::Failed(
            "decoder output has odd dimensions".into(),
        ));
    }
    let mut planes = [(ptr::null_mut::<u8>(), 0usize, 0usize); 2];
    status(
        // SAFETY: Live two-plane buffer, balanced read-only lock below.
        unsafe { CVPixelBufferLockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) },
        "CVPixelBufferLockBaseAddress NV12",
    )?;
    let copied = (|| {
        for (index, plane) in planes.iter_mut().enumerate() {
            let stride = CVPixelBufferGetBytesPerRowOfPlane(image, index);
            let height = CVPixelBufferGetHeightOfPlane(image, index);
            let width = CVPixelBufferGetWidthOfPlane(image, index);
            let len = stride
                .checked_mul(height)
                .filter(|&len| len <= isize::MAX as usize)
                .ok_or_else(|| missing("valid NV12 extent"))?;
            let base = CVPixelBufferGetBaseAddressOfPlane(image, index).cast::<u8>();
            if base.is_null()
                || stride < size.width as usize
                || stride > u32::MAX as usize
                || height != size.height as usize / (index + 1)
                || width != size.width as usize / (index + 1)
            {
                return Err(missing("valid NV12 plane storage"));
            }
            *plane = (base, stride, len);
        }
        out.y
            .try_reserve(planes[0].2.saturating_sub(out.y.len()))
            .map_err(|_| CodecError::Failed("NV12 luma allocation failed".into()))?;
        out.uv
            .try_reserve(planes[1].2.saturating_sub(out.uv.len()))
            .map_err(|_| CodecError::Failed("NV12 chroma allocation failed".into()))?;
        for (destination, &(base, _, len)) in [&mut out.y, &mut out.uv].into_iter().zip(&planes) {
            destination.clear();
            // SAFETY: CoreVideo's locked plane has stride * height bytes; geometry and
            // address were checked above. Copy completes before unlock; no pointer escapes.
            destination.extend_from_slice(unsafe { std::slice::from_raw_parts(base, len) });
        }
        out.size = size;
        out.y_stride = planes[0].1 as u32;
        out.uv_stride = planes[1].1 as u32;
        out.colour = image_colour(image);
        Ok(())
    })();
    // SAFETY: Balances successful read-only lock; source slices no longer exist.
    let unlocked = status(
        unsafe { CVPixelBufferUnlockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) },
        "CVPixelBufferUnlockBaseAddress NV12",
    );
    copied?;
    unlocked
}

fn copy_image(image: &CVPixelBuffer) -> Result<(PixelSize, Vec<u8>), CodecError> {
    if CVPixelBufferGetPixelFormatType(image) != kCVPixelFormatType_32BGRA {
        return Err(CodecError::Failed("decoder output is not BGRA".into()));
    }
    let width = CVPixelBufferGetWidth(image);
    let height = CVPixelBufferGetHeight(image);
    let size = decode_size(
        i32::try_from(width).map_err(|_| missing("valid width"))?,
        i32::try_from(height).map_err(|_| missing("valid height"))?,
    )?;
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
        let mut out = Vec::new();
        out.try_reserve_exact(width * 4 * height)
            .map_err(|_| CodecError::Failed("decoded image allocation failed".into()))?;
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

fn decode_size(width: i32, height: i32) -> Result<PixelSize, CodecError> {
    if width <= 0
        || height <= 0
        || width > MAX_DECODE_DIMENSION as i32
        || height > MAX_DECODE_DIMENSION as i32
    {
        return Err(CodecError::Failed(
            "H.264 dimensions exceed the 8192-per-axis limit or are invalid".into(),
        ));
    }
    Ok(PixelSize::new(width as u32, height as u32))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn moving_pattern(size: PixelSize, frame: u32) -> Vec<u8> {
        let mut pixels = Vec::new();
        for y in 0..size.height {
            for x in 0..size.width {
                let value = if (x / 16 + frame / 2) % 8 < 3 && (y / 16 + frame / 3) % 6 < 3 {
                    [48, 150, 208, 255]
                } else {
                    [80, 96, 112, 255]
                };
                pixels.extend_from_slice(&value);
            }
        }
        pixels
    }

    fn surface_buffer(size: PixelSize) -> CFRetained<CVPixelBuffer> {
        let attributes = bgra_attributes(size);
        let mut raw = ptr::null_mut();
        // SAFETY: Valid dimensions, IOSurface attributes and writable output pointer.
        assert_eq!(
            // SAFETY: Valid IOSurface attributes, BGRA dimensions and writable out pointer.
            unsafe {
                CVPixelBufferCreate(
                    None,
                    size.width as usize,
                    size.height as usize,
                    kCVPixelFormatType_32BGRA,
                    Some(attributes.as_ref()),
                    NonNull::from(&mut raw),
                )
            },
            0
        );
        // SAFETY: Successful Create transfers its +1 reference.
        unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) }
    }

    fn quality(source: &[u8], decoded: &[u8], size: PixelSize, coded: PixelSize) -> f64 {
        let mut error = 0.0;
        for y in 0..size.height as usize {
            for x in 0..size.width as usize {
                for c in 0..3 {
                    let delta = f64::from(source[(y * size.width as usize + x) * 4 + c])
                        - f64::from(decoded[(y * coded.width as usize + x) * 4 + c]);
                    error += delta * delta;
                }
            }
        }
        10.0 * (255.0 * 255.0 * f64::from(size.width) * f64::from(size.height) * 3.0 / error)
            .log10()
    }

    fn fill_nv12(input: &PoolInput, pixels: &[u8], size: PixelSize) {
        let buffer = &input.buffer;
        // SAFETY: Test exclusively owns input and completes all writes before encoding.
        assert_eq!(
            // SAFETY: Test-owned buffer; writes finish before encoding and unlocking.
            unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::empty()) },
            0
        );
        let y_base = CVPixelBufferGetBaseAddressOfPlane(buffer, 0).cast::<u8>();
        let uv_base = CVPixelBufferGetBaseAddressOfPlane(buffer, 1).cast::<u8>();
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1);
        let rgb = |x: u32, y: u32| {
            let offset = (y.min(size.height - 1) * size.width + x.min(size.width - 1)) as usize * 4;
            let p = &pixels[offset..offset + 4];
            let (r, g, b) = (f64::from(p[2]), f64::from(p[1]), f64::from(p[0]));
            let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            [
                16.0 + luma * 219.0 / 255.0,
                128.0 + (b - luma) / (2.0 * (1.0 - 0.0722)) * 224.0 / 255.0,
                128.0 + (r - luma) / (2.0 * (1.0 - 0.2126)) * 224.0 / 255.0,
            ]
        };
        for y in 0..input.size.height {
            for x in 0..input.size.width {
                // SAFETY: Checked pool geometry, locked luma plane, in-bounds row/column.
                unsafe {
                    *y_base.add(y as usize * y_stride + x as usize) = rgb(x, y)[0].round() as u8;
                }
            }
        }
        for y in (0..input.size.height).step_by(2) {
            for x in (0..input.size.width).step_by(2) {
                let values = [rgb(x, y), rgb(x + 1, y), rgb(x, y + 1), rgb(x + 1, y + 1)];
                for c in 1..3 {
                    let value = values.iter().map(|v| v[c]).sum::<f64>() / 4.0;
                    // SAFETY: Locked interleaved chroma plane; coded dimensions are even.
                    unsafe {
                        *uv_base.add(y as usize / 2 * uv_stride + x as usize + c - 1) =
                            value.round() as u8;
                    }
                }
            }
        }
        // SAFETY: Balances the write lock after all plane writes complete.
        assert_eq!(
            // SAFETY: Balances the successful write lock above.
            unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::empty()) },
            0
        );
    }

    #[test]
    fn native_bgra_and_crops_match_cpu_quality() {
        use crosspane_types::geom::PixelRect;
        for (size, crop) in [
            (PixelSize::new(160, 96), false),
            (PixelSize::new(159, 95), false),
            (PixelSize::new(160, 96), true),
            (PixelSize::new(159, 95), true),
        ] {
            let Some(mut native) = available(Encoder::new(size, 8_000_000, 30)) else {
                return;
            };
            let mut cpu = Encoder::new(size, 8_000_000, 30).unwrap();
            let mut native_decoder = Decoder::new(true).unwrap();
            let mut cpu_decoder = Decoder::new(true).unwrap();
            let mut packet = Vec::new();
            let mut decoded = Vec::new();
            let full = PixelSize::new(
                size.width + if crop { 32 } else { 0 },
                size.height + if crop { 24 } else { 0 },
            );
            let (left, top) = if crop { (8, 4) } else { (0, 0) };
            let mut scores = [0.0, 0.0];
            for frame in 0..60 {
                let source = moving_pattern(size, frame);
                let mut full_pixels = vec![0; full.width as usize * full.height as usize * 4];
                for y in 0..size.height as usize {
                    let offset = ((y + top) * full.width as usize + left) * 4;
                    let row = size.width as usize * 4;
                    full_pixels[offset..offset + row]
                        .copy_from_slice(&source[y * row..(y + 1) * row]);
                }
                let buffer = fill_input_buffer(
                    surface_buffer(full),
                    &full_pixels,
                    full.width as usize * 4,
                    full,
                    full,
                )
                .unwrap();
                let image: Arc<dyn NativeImage> = Arc::new(SckImage::new(
                    buffer,
                    PixelRect::new(
                        [left as i32, top as i32].into(),
                        [
                            left as i32 + size.width as i32,
                            top as i32 + size.height as i32,
                        ]
                        .into(),
                    ),
                ));
                let input = crate::frame_capture::capture_input(&image).unwrap();
                let force = frame == 30;
                let result = native
                    .encode_native(&*input, size, force, &mut packet)
                    .unwrap();
                assert_eq!(result.key, frame == 0 || force);
                let coded = native_decoder.decode(&packet, &mut decoded).unwrap();
                assert_eq!(coded, coded_size(size).unwrap());
                let native_score = quality(&source, &decoded, size, coded);
                scores[0] += native_score;
                cpu.encode(&source, size.width * 4, size, force, &mut packet)
                    .unwrap();
                let coded = cpu_decoder.decode(&packet, &mut decoded).unwrap();
                let cpu_score = quality(&source, &decoded, size, coded);
                scores[1] += cpu_score;
                assert!(
                    (native_score - cpu_score).abs() <= 0.5,
                    "BGRA frame {frame}: native={native_score}, CPU={cpu_score}"
                );
            }
            eprintln!(
                "native BGRA {size:?} crop={crop}: PSNR native={} CPU={}",
                scores[0] / 60.0,
                scores[1] / 60.0
            );
            assert!((scores[0] - scores[1]).abs() / 60.0 <= 0.5);
        }
    }

    #[test]
    fn native_nv12_quality_exhaustion_and_switch_to_bgra() {
        for size in [PixelSize::new(160, 96), PixelSize::new(159, 95)] {
            let Some(mut encoder) = available(Encoder::new(size, 8_000_000, 30)) else {
                return;
            };
            let Some(pool) = available(encoder.input_pool(size)) else {
                return;
            };
            let pool = pool.unwrap();
            assert_eq!(pool.size(), coded_size(size).unwrap());
            let mut held = Vec::new();
            loop {
                match pool.acquire() {
                    Ok(input) => {
                        held.push(input);
                        assert!(held.len() <= 8);
                    }
                    Err(CodecError::Failed(reason)) => {
                        eprintln!("pool exhausted with {} held: {reason}", held.len());
                        break;
                    }
                    Err(error) => panic!("unexpected pool error: {error}"),
                }
            }
            assert!(!held.is_empty());
            assert!(matches!(pool.acquire(), Err(CodecError::Failed(_))));
            held.pop();
            let freed = pool.acquire().unwrap();
            drop(freed);
            drop(held);
            let mut cpu = Encoder::new(size, 8_000_000, 30).unwrap();
            let mut decoder = Decoder::new(true).unwrap();
            let mut cpu_decoder = Decoder::new(true).unwrap();
            let mut packet = Vec::new();
            let mut decoded = Vec::new();
            let mut scores = [0.0, 0.0];
            for frame in 0..60 {
                let pixels = moving_pattern(size, frame);
                let input = pool.acquire().unwrap();
                assert_eq!(
                    input.colour(),
                    YuvColour {
                        matrix: YuvMatrix::Bt709,
                        full_range: false
                    }
                );
                fill_nv12(
                    input.as_any().downcast_ref::<PoolInput>().unwrap(),
                    &pixels,
                    size,
                );
                let result = encoder
                    .encode_native(&*input, size, frame == 30, &mut packet)
                    .unwrap();
                assert_eq!(result.key, frame == 0 || frame == 30);
                let coded = decoder.decode(&packet, &mut decoded).unwrap();
                let native_score = quality(&pixels, &decoded, size, coded);
                scores[0] += native_score;
                cpu.encode(&pixels, size.width * 4, size, frame == 30, &mut packet)
                    .unwrap();
                let coded = cpu_decoder.decode(&packet, &mut decoded).unwrap();
                let cpu_score = quality(&pixels, &decoded, size, coded);
                scores[1] += cpu_score;
                // VT's own BGRA conversion and the test's reference NV12 differ slightly per
                // frame; the average below must stay within 0.5 dB.
                assert!(
                    (native_score - cpu_score).abs() <= 1.0,
                    "NV12 frame {frame}: native={native_score}, CPU={cpu_score}"
                );
            }
            eprintln!(
                "NV12 {size:?}: PSNR native={} CPU={}",
                scores[0] / 60.0,
                scores[1] / 60.0
            );
            assert!((scores[0] - scores[1]).abs() / 60.0 <= 0.5);
            let stale = pool.acquire().unwrap();
            let pixels = moving_pattern(size, 0);
            assert!(
                encoder
                    .encode(&pixels, size.width * 4, size, false, &mut packet)
                    .unwrap()
                    .key
            );
            assert!(matches!(
                encoder.encode_native(&*stale, size, false, &mut packet),
                Err(CodecError::BadInput(_))
            ));
            assert!(packet.is_empty());
        }
    }

    fn available<T>(result: Result<T, CodecError>) -> Option<T> {
        match result {
            Ok(codec) => Some(codec),
            Err(CodecError::Unavailable(reason)) => {
                eprintln!("skipped: VideoToolbox unavailable: {reason}");
                None
            }
            Err(error) => panic!("VideoToolbox factory: {error}"),
        }
    }

    #[derive(Default)]
    struct Writer(Vec<bool>);

    impl Writer {
        fn bit(&mut self, value: bool) {
            self.0.push(value);
        }

        fn uint(&mut self, value: u32, count: u32) {
            for bit in (0..count).rev() {
                self.bit(value & (1 << bit) != 0);
            }
        }

        fn ue(&mut self, value: u32) {
            let code = value + 1;
            let count = 32 - code.leading_zeros();
            for _ in 1..count {
                self.bit(false);
            }
            self.uint(code, count);
        }

        fn nal(mut self, header: u8) -> Vec<u8> {
            self.bit(true); // rbsp_stop_one_bit
            while !self.0.len().is_multiple_of(8) {
                self.bit(false);
            }
            let mut out = vec![header];
            let mut zeros = 0;
            for bits in self.0.as_chunks::<8>().0 {
                let byte = bits
                    .iter()
                    .fold(0u8, |value, bit| value << 1 | u8::from(*bit));
                if zeros == 2 && byte <= 3 {
                    out.push(3);
                    zeros = 0;
                }
                out.push(byte);
                zeros = if byte == 0 { zeros + 1 } else { 0 };
            }
            out
        }
    }

    fn sps(width_in_mbs: u32, scaling_lists: bool) -> Vec<u8> {
        let mut bits = Writer::default();
        bits.uint(100, 8); // High profile
        bits.uint(0, 8); // constraint flags
        bits.uint(51, 8); // level
        bits.ue(0); // SPS id
        bits.ue(1); // 4:2:0
        bits.ue(0);
        bits.ue(0); // 8-bit luma/chroma
        bits.bit(false); // transform bypass
        bits.bit(scaling_lists);
        if scaling_lists {
            for index in 0..8 {
                bits.bit(true);
                for _ in 0..if index < 6 { 16 } else { 64 } {
                    bits.ue(0);
                } // signed delta_scale = 0
            }
        }
        bits.ue(0); // log2_max_frame_num_minus4
        bits.ue(2); // pic_order_cnt_type
        bits.ue(1); // one reference frame
        bits.bit(false); // no frame_num gaps
        bits.ue(width_in_mbs - 1);
        bits.ue(1); // 32 pixels high
        bits.bit(true); // frame_mbs_only
        bits.bit(true); // direct_8x8_inference
        bits.bit(false); // no cropping
        bits.bit(false); // no VUI
        bits.nal(0x67)
    }

    fn pps() -> Vec<u8> {
        let mut bits = Writer::default();
        bits.ue(0);
        bits.ue(0); // PPS/SPS ids
        bits.bit(false);
        bits.bit(false); // CAVLC, no bottom-field POC
        bits.ue(0);
        bits.ue(0);
        bits.ue(0); // one slice group, default reference counts
        bits.bit(false);
        bits.uint(0, 2); // no weighted prediction
        bits.ue(0);
        bits.ue(0);
        bits.ue(0); // signed QP/QS/chroma offsets = 0
        bits.bit(true);
        bits.bit(false);
        bits.bit(false); // deblocking, constrained intra, redundant pictures
        bits.nal(0x68)
    }

    fn unit(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
        out
    }

    fn slice(header: u8, slice_type: u32, frame_num: u32, frame_num_bits: u32) -> Vec<u8> {
        let mut bits = Writer::default();
        bits.ue(0);
        bits.ue(slice_type);
        bits.ue(0); // first MB, slice type, PPS id
        bits.uint(frame_num, frame_num_bits);
        bits.nal(header)
    }

    fn encoded_fixture() -> Option<Vec<u8>> {
        let size = PixelSize::new(32, 32);
        let mut encoder = available(VtCodecs::new().encoder(size, 8_000_000, 30))?;
        let pixels = [48, 150, 208, 255].repeat(32 * 32);
        let mut packet = Vec::new();
        assert!(
            encoder
                .encode(&pixels, 128, size, false, &mut packet)
                .unwrap()
                .key
        );
        Some(packet)
    }

    #[test]
    fn negative_annex_b_inputs() {
        let cases: &[(&str, &[u8])] = &[
            ("empty", &[]),
            ("no start code", &[0x65, 0x80]),
            ("garbage prefix", &[9, 0, 0, 1, 0x65]),
            ("empty NAL", &[0, 0, 1, 0, 0, 1]),
            ("forbidden bit", &[0, 0, 1, 0xe5, 0x80]),
        ];
        for (name, data) in cases {
            let mut decoder = Decoder::new(true).unwrap();
            let mut out = vec![11, 22, 33];
            assert!(decoder.decode(data, &mut out).is_err(), "{name}");
            assert_eq!(out, [11, 22, 33], "{name}");
        }
        for avcc in [
            &[][..],
            &[0, 0, 0][..],
            &[0, 0, 0, 0][..],
            &[0, 0, 0, 2, 0x65][..],
        ] {
            assert!(avcc_nals(avcc).is_err());
        }
        let nals = avcc_nals(&[0, 0, 0, 2, 0x65, 0x80]).unwrap();
        assert_eq!(nals[0][0] & 31, 5);
    }

    #[test]
    fn negative_sps_and_scaling_lists() {
        for sps in [
            &[][..],
            &[0x67][..],
            &[0xff; 16][..],
            &[0x67, 0xff, 0xff, 0xff, 0xff][..],
        ] {
            assert!(frame_num_bits(sps).is_err(), "invalid SPS {sps:?}");
            let mut decoder = Decoder::new(true).unwrap();
            assert!(
                decoder
                    .decode(&unit(&[sps, &pps(), &[0x65]]), &mut Vec::new())
                    .is_err()
            );
        }
        let sps = sps(2, true);
        assert!(sps.len() > 32, "scaling lists exceed a slice-sized prefix");
        assert_eq!(frame_num_bits(&sps).unwrap(), 4);
        let mut decoder = Decoder::new(true).unwrap();
        assert!(
            decoder
                .decode(&unit(&[&sps, &pps(), &[0x65]]), &mut Vec::new())
                .is_err()
        );
        assert!(
            frame_num_bits(&sps[..32]).is_err(),
            "truncated scaling lists"
        );
        let mut decoder = Decoder::new(true).unwrap();
        assert!(
            decoder
                .decode(&unit(&[&sps[..32], &pps(), &[0x65]]), &mut Vec::new())
                .is_err()
        );
    }

    #[test]
    fn negative_access_units() {
        let Some(packet) = encoded_fixture() else {
            return;
        };
        let nals = annex_b_nals(&packet).unwrap();
        let sps = nals.iter().find(|nal| nal[0] & 31 == 7).unwrap();
        let pps = nals.iter().find(|nal| nal[0] & 31 == 8).unwrap();
        let frame_num_bits = frame_num_bits(sps).unwrap();
        let b = slice(0x65, 1, 0, frame_num_bits);
        let first = slice(0x65, 2, 0, frame_num_bits);
        let cases = [
            (
                "truncated slice header",
                unit(&[sps, pps, &[0x65]]),
                "truncated H.264 header",
            ),
            (
                "B-slice",
                unit(&[sps, pps, &b]),
                "B-frames are not supported",
            ),
            ("SPS-only", unit(&[sps]), "slice before SPS/PPS"),
            (
                "two pictures",
                unit(&[sps, pps, &first, &first]),
                "multiple pictures in one access unit",
            ),
        ];
        for (name, data, message) in cases {
            let mut decoder = Decoder::new(true).unwrap();
            let mut out = vec![11, 22, 33];
            let error = decoder.decode(&data, &mut out).unwrap_err();
            if let CodecError::Unavailable(reason) = error {
                eprintln!("skipped: {reason}");
                return;
            }
            assert!(error.to_string().contains(message), "{name}: {error}");
            assert_eq!(out, [11, 22, 33], "{name}");
        }
    }

    #[test]
    fn mutated_real_access_units_do_not_panic() {
        let Some(packet) = encoded_fixture() else {
            return;
        };
        let Some(mut decoder) = available(VtCodecs::new().decoder()) else {
            return;
        };
        let mut out = Vec::new();
        for index in 0..512 {
            let mut data = packet.clone();
            if index % 2 == 0 {
                let position = index / 2 % data.len();
                data[position] ^= 1 << (index / (2 * data.len()) % 8);
            } else {
                data.truncate(index / 2 % data.len());
            }
            let result = catch_unwind(AssertUnwindSafe(|| decoder.decode(&data, &mut out)));
            assert!(result.is_ok(), "mutation {index} panicked");
        }
        eprintln!(
            "512 real access-unit mutations (bit flips/truncations) returned without panicking"
        );
    }

    #[test]
    fn factory_availability() {
        assert!(matches!(
            Decoder::new(false),
            Err(CodecError::Unavailable(_))
        ));
        assert!(matches!(
            create_status(-12908, "VTCompressionSessionCreate"),
            Err(CodecError::Unavailable(_))
        ));
        assert!(create_status(0, "VTCompressionSessionCreate").is_ok());
        // SAFETY: Public H.264 capability probe, with no session or callback involved.
        let supported = unsafe { VTIsHardwareDecodeSupported(kCMVideoCodecType_H264) };
        assert_eq!(VtCodecs::new().decoder().is_ok(), supported);
        let _ = available(VtCodecs::new().encoder(PixelSize::new(32, 32), 8_000_000, 30));
    }

    #[test]
    fn pool_attributes_and_transfer() {
        let Some(encoder) = available(Encoder::new(PixelSize::new(250, 142), 20_000_000, 30))
        else {
            return;
        };
        // SAFETY: Live prepared test session; public pool getter retains its result.
        let pool = unsafe { encoder.session.as_deref().unwrap().pixel_buffer_pool() }.unwrap();
        let image = pool_buffer(&pool).unwrap();
        assert_eq!(
            CVPixelBufferGetPixelFormatType(&image),
            kCVPixelFormatType_32BGRA
        );
        assert_eq!(CVPixelBufferGetWidth(&image), 250);
        assert_eq!(CVPixelBufferGetHeight(&image), 142);
        eprintln!("pool: BGRA 250x142; transfer={}", encoder.transfer);
    }

    #[test]
    fn decode_dimensions_capped_before_session_creation() {
        assert_eq!(decode_size(8192, 8192).unwrap(), PixelSize::new(8192, 8192));
        for (width, height) in [(8193, 32), (32, 8193), (0, 32), (-1, 32)] {
            assert!(matches!(
                decode_size(width, height),
                Err(CodecError::Failed(_))
            ));
        }
        let error = DecodeSession::new(&sps(513, false), &pps()).err().unwrap();
        assert!(matches!(error, CodecError::Failed(_)));
        assert!(
            error.to_string().contains("8192"),
            "dimension check must happen before allocating a VT session: {error}"
        );
    }

    #[test]
    fn bitrate_and_clean_restart() {
        let size = PixelSize::new(32, 32);
        let Some(mut encoder) = available(Encoder::new(size, 8_000_000, 30)) else {
            return;
        };
        let pixels = [48, 150, 208, 255].repeat(32 * 32);
        let mut out = Vec::new();
        encoder.encode(&pixels, 128, size, false, &mut out).unwrap();
        encoder.set_bitrate(0);
        assert_eq!(encoder.bitrate, 8_000_000);
        encoder.set_bitrate(1_000_000);
        assert_eq!(encoder.applied_bitrate, 8_000_000);
        encoder.encode(&pixels, 128, size, false, &mut out).unwrap();
        assert_eq!(encoder.applied_bitrate, 1_000_000);
        encoder.encode(&pixels, 128, size, false, &mut out).unwrap();
        assert_eq!(encoder.applied_bitrate, 1_000_000);
        let image = input_buffer(&pixels, 128, size, size).unwrap();
        let timestamp = CMTime {
            value: 3,
            timescale: 30,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        status(
            // SAFETY: Live test-owned session and image; the boxed refcon remains live. Submit one
            // frame without CompleteFrames so reset must drain any callback before invalidation.
            unsafe {
                encoder.session.as_deref().unwrap().encode_frame(
                    &image,
                    timestamp,
                    CMTime {
                        value: 1,
                        ..timestamp
                    },
                    None,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            "test pending encode",
        )
        .unwrap();
        encoder.reset();
        assert!(encoder.session.is_none());
        assert!(encoder.output.try_recv().is_err());
        assert!(encoder.first);
        // Failed attempts leave the reset state ready to retry, then the next valid input is IDR.
        assert!(
            encoder
                .encode(&[], 0, PixelSize::new(0, 0), false, &mut out)
                .is_err()
        );
        assert!(encoder.session.is_none());
        let result = match encoder.encode(&pixels, 128, size, false, &mut out) {
            Err(CodecError::Unavailable(reason)) => {
                eprintln!("skipped: restart unavailable: {reason}");
                return;
            }
            result => result.unwrap(),
        };
        assert!(result.key);
    }

    #[test]
    fn unknown_hardware_diagnostic_is_non_fatal() {
        let Some(packet) = encoded_fixture() else {
            return;
        };
        let nals = annex_b_nals(&packet).unwrap();
        let sps = nals.iter().find(|nal| nal[0] & 31 == 7).unwrap();
        let pps = nals.iter().find(|nal| nal[0] & 31 == 8).unwrap();
        let Some(mut session) = available(DecodeSession::new(sps, pps)) else {
            return;
        };
        let unknown = CFString::from_str("CrosspaneUnsupportedHardwareDiagnostic");
        session.hardware = hardware(session.session.as_ref(), &unknown).ok();
        assert_eq!(session.hardware, None);
        let mut decoder = Decoder::new(true).unwrap();
        decoder.sps = sps.to_vec();
        decoder.pps = pps.to_vec();
        decoder.session = Some(session);
        assert_eq!(
            decoder.decode(&packet, &mut Vec::new()).unwrap(),
            PixelSize::new(32, 32)
        );
        assert!(decoder.name().contains("status unknown"));
    }
}
