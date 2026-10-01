use std::any::Any;
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak, mpsc};
use std::time::Instant;

use block2::RcBlock;
use crosspane_media::codec::NativeInput;
use crosspane_media::picture::{YuvColour, YuvMatrix};
use crosspane_platform::{
    CaptureTarget, Frame, FrameImage, NativeImage, Permission, PlatformError, StreamEndReason,
    StreamId,
};
use crosspane_types::geom::{PixelRect, PixelSize};
use crosspane_types::id::DisplayId;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{
    CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, CGRectMakeWithDictionaryRepresentation,
    kCGColorSpaceSRGB,
};
use objc2_core_media::{CMClock, CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetDataSize, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamErrorCode, SCStreamErrorDomain, SCStreamFrameInfoDirtyRects,
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use super::{Command, Shared};
use crate::clock;

pub(super) fn receive<T>(rx: &mpsc::Receiver<T>, deadline: Instant) -> Result<T, PlatformError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(PlatformError::Timeout)?;
    rx.recv_timeout(remaining).map_err(|e| match e {
        mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
        mpsc::RecvTimeoutError::Disconnected => {
            PlatformError::Backend("ScreenCaptureKit callback disconnected".into())
        }
    })
}

/// Native references stay on the worker. During an OS completion wait, keep observing the gate
/// so an idle stream is stopped even if SCK takes the full two seconds to answer enumeration.
pub(super) struct Wait<'a> {
    shared: &'a Shared,
    streams: Vec<Retained<SCStream>>,
}

impl<'a> Wait<'a> {
    pub fn new<'s>(shared: &'a Shared, streams: impl Iterator<Item = &'s Stream>) -> Self {
        Self {
            shared,
            streams: streams.map(|stream| stream.stream.clone()).collect(),
        }
    }

    fn check(&self, starting: Option<&SCStream>) -> Result<(), PlatformError> {
        if self.shared.permitted() {
            return Ok(());
        }
        self.block(starting);
        super::check_permission()?;
        Err(PlatformError::Locked)
    }

    fn block(&self, starting: Option<&SCStream>) {
        // SAFETY: These retained SCK streams stay on the worker. Stop is asynchronous and has no
        // completion wait; submit all stops before ending all deliveries. No AppKit is involved.
        unsafe {
            for stream in &self.streams {
                stream.stopCaptureWithCompletionHandler(None);
            }
            if let Some(stream) = starting {
                stream.stopCaptureWithCompletionHandler(None);
            }
        }
        self.shared.end_all(StreamEndReason::Blocked);
    }

    fn check_result<T>(
        &self,
        result: Result<T, PlatformError>,
        starting: Option<&SCStream>,
    ) -> Result<T, PlatformError> {
        // Preflight can be stale after TCC changes; SCK's explicit denial is also authoritative.
        if matches!(
            result,
            Err(PlatformError::PermissionDenied(Permission::ScreenRecording))
        ) {
            self.block(starting);
        }
        result
    }

    pub fn receive<T>(
        &self,
        rx: &mpsc::Receiver<T>,
        deadline: Instant,
        starting: Option<&SCStream>,
    ) -> Result<T, PlatformError> {
        loop {
            self.check(starting)?;
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(PlatformError::Timeout)?;
            match rx.recv_timeout(remaining.min(super::POLL)) {
                Ok(value) => {
                    self.check(starting)?;
                    return Ok(value);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(PlatformError::Backend(
                        "ScreenCaptureKit callback disconnected".into(),
                    ));
                }
            }
        }
    }
}

// An immutable enumeration snapshot, transferred once from SCK's completion thread to the worker.
pub(super) struct Content(pub Retained<SCShareableContent>);
// SAFETY: SCShareableContent is a read-only snapshot designed for asynchronous enumeration.
// Retain happens in its completion callback; only the receiving worker reads it afterwards.
unsafe impl Send for Content {}

fn error_result(error: *mut NSError) -> Result<(), PlatformError> {
    // SAFETY: SCK provides either null or a live NSError for the duration of its callback.
    if let Some(error) = unsafe { error.as_ref() } {
        if permission_error(error) {
            return Err(PlatformError::PermissionDenied(Permission::ScreenRecording));
        }
        // Never copy localized descriptions (which can include window titles) into logs.
        Err(PlatformError::Backend(format!(
            "ScreenCaptureKit {} code {}",
            error.domain(),
            error.code()
        )))
    } else {
        Ok(())
    }
}

fn permission_error(error: &NSError) -> bool {
    // SAFETY: The framework exports an immutable NSString error-domain constant.
    &*error.domain() == unsafe { SCStreamErrorDomain }
        && error.code() == SCStreamErrorCode::UserDeclined.0
}

pub(super) fn content(deadline: Instant, wait: &Wait<'_>) -> Result<Content, PlatformError> {
    wait.check(None)?;
    if Instant::now() >= deadline {
        return Err(PlatformError::Timeout);
    }
    let (tx, rx) = mpsc::channel();
    let callback = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = error_result(error).and_then(|()| {
                // SAFETY: The nullable SCK completion argument is live during this callback. Retain
                // before transferring the immutable snapshot; no borrowed pointer escapes.
                unsafe { Retained::retain(content) }
                    .map(Content)
                    .ok_or_else(|| {
                        PlatformError::Backend("ScreenCaptureKit returned no content".into())
                    })
            });
            let _ = tx.send(result);
        },
    );
    // SAFETY: The block has the documented completion signature; SCK copies it for async use.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&callback) };
    wait.check_result(wait.receive(&rx, deadline, None)?, None)
}

struct OutputIvars {
    shared: Weak<Shared>,
    id: StreamId,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. Ivars contain only thread-safe Rust
    // state; callbacks borrow native arguments only during the call. No custom Drop.
    #[unsafe(super(NSObject))]
    #[name = "CrosspaneFrameOutput"]
    #[ivars = OutputIvars]
    struct Output;

    // SAFETY: NSObjectProtocol adds no requirements.
    unsafe impl NSObjectProtocol for Output {}

    // SAFETY: The selector and argument types match SCStreamOutput. The supplied serial queue
    // serializes samples; Shared serializes these with control/delegate end events.
    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn sample(&self, stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Screen
                && let Some(shared) = self.ivars().shared.upgrade()
            {
                shared.frame(self.ivars().id, sample);
                if !shared.permitted() {
                    // SAFETY: SCK allows asynchronous control from a sample handler; no wait,
                    // and no frame or native reference escapes this callback.
                    unsafe { stream.stopCaptureWithCompletionHandler(None) };
                }
            }
        }
    }

    // SAFETY: The selector and argument types match SCStreamDelegate. This callback queues
    // classification to the worker, never waits on it and never touches AppKit.
    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn failed(&self, _stream: &SCStream, error: &NSError) {
            if let Some(shared) = self.ivars().shared.upgrade() {
                if !shared.permitted() || permission_error(error) {
                    shared.end_all(StreamEndReason::Blocked);
                }
                let _ = shared.commands.send(Command::Failed(self.ivars().id));
            }
        }
    }
);

impl Output {
    fn new(shared: &Arc<Shared>, id: StreamId) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars {
            shared: Arc::downgrade(shared),
            id,
        });
        // SAFETY: NSObject init takes no arguments; the Rust ivars are initialized.
        unsafe { msg_send![super(this), init] }
    }
}

pub(super) struct Stream {
    pub target: CaptureTarget,
    pub scale: f64,
    size: PixelSize,
    crop: Option<PixelRect>,
    fps: u32,
    stream: Retained<SCStream>,
    // Keep the weak delegate and sample queue alive for the native stream's entire lifetime.
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
}

fn filter(
    content: &SCShareableContent,
    target: CaptureTarget,
) -> Result<(Retained<SCContentFilter>, PixelSize, f64), PlatformError> {
    // SAFETY: These are immutable SCK snapshot properties and documented filter initializers.
    // Arrays and matched snapshot objects stay retained until the initializer returns.
    let (filter, rect) = unsafe {
        match target {
            CaptureTarget::Window(id) => {
                let windows = content.windows();
                let window = windows
                    .iter()
                    .find(|w| u64::from(w.windowID()) == id.0)
                    .ok_or(PlatformError::NotFound)?;
                (
                    SCContentFilter::initWithDesktopIndependentWindow(
                        SCContentFilter::alloc(),
                        &window,
                    ),
                    window.frame(),
                )
            }
            CaptureTarget::Display(id) => {
                let displays = content.displays();
                let display = displays
                    .iter()
                    .find(|d| d.displayID() == id.0)
                    .ok_or(PlatformError::NotFound)?;
                (
                    SCContentFilter::initWithDisplay_excludingWindows(
                        SCContentFilter::alloc(),
                        &display,
                        &NSArray::<SCWindow>::new(),
                    ),
                    display.frame(),
                )
            }
            _ => return Err(PlatformError::Unsupported("unknown capture target")),
        }
    };
    if let CaptureTarget::Display(id) = target {
        let (size, scale) = display_geometry(id)?;
        return Ok((filter, size, scale));
    }
    // SAFETY: Read-only property of the live, initialized filter; gives the target's backing scale.
    let scale = f64::from(unsafe { filter.pointPixelScale() });
    let width = (rect.size.width * scale).round();
    let height = (rect.size.height * scale).round();
    if !scale.is_finite()
        || scale <= 0.0
        || !width.is_finite()
        || !height.is_finite()
        || width < 1.0
        || height < 1.0
        || width > f64::from(i32::MAX)
        || height > f64::from(i32::MAX)
    {
        return Err(PlatformError::Backend("invalid capture geometry".into()));
    }
    Ok((filter, PixelSize::new(width as u32, height as u32), scale))
}

fn display_geometry(id: DisplayId) -> Result<(PixelSize, f64), PlatformError> {
    let mode = CGDisplayCopyDisplayMode(id.0).ok_or(PlatformError::NotFound)?;
    let width = CGDisplayMode::pixel_width(Some(&mode));
    let height = CGDisplayMode::pixel_height(Some(&mode));
    let scale = width as f64 / CGDisplayMode::width(Some(&mode)) as f64;
    if width == 0
        || height == 0
        || width > i32::MAX as usize
        || height > i32::MAX as usize
        || !scale.is_finite()
        || scale <= 0.0
    {
        return Err(PlatformError::Backend("invalid capture geometry".into()));
    }
    Ok((PixelSize::new(width as u32, height as u32), scale))
}

pub(super) fn target_exists(content: &SCShareableContent, target: CaptureTarget) -> bool {
    // SAFETY: Read-only IDs and retained arrays from the immutable enumeration snapshot.
    unsafe {
        match target {
            CaptureTarget::Window(id) => content
                .windows()
                .iter()
                .any(|w| u64::from(w.windowID()) == id.0),
            CaptureTarget::Display(id) => content.displays().iter().any(|d| d.displayID() == id.0),
            _ => false,
        }
    }
}

fn checked_crop(crop: Option<PixelRect>, size: PixelSize) -> Result<PixelRect, PlatformError> {
    let bounds = PixelRect::new(
        [0, 0].into(),
        [size.width as i32, size.height as i32].into(),
    );
    let rect = crop.unwrap_or(bounds);
    if rect.is_empty() || !bounds.contains_box(&rect) {
        Err(PlatformError::Backend(
            "crop must be a nonempty rectangle inside the target".into(),
        ))
    } else {
        Ok(rect)
    }
}

fn crop_needs_refresh(target: CaptureTarget, crop: Option<PixelRect>, size: PixelSize) -> bool {
    matches!(target, CaptureTarget::Display(_)) && checked_crop(crop, size).is_err()
}

fn configuration(
    target: CaptureTarget,
    size: PixelSize,
    scale: f64,
    crop: Option<PixelRect>,
    fps: u32,
) -> Result<Retained<SCStreamConfiguration>, PlatformError> {
    let rect = checked_crop(crop, size)?;
    // SAFETY: All setters are documented SCK APIs on a new, worker-owned configuration. FPS was
    // validated before construction; dimensions and sourceRect have been checked against bounds.
    // The public sRGB name is an immutable framework constant, alive for the stream's lifetime.
    let config = unsafe {
        let config = SCStreamConfiguration::new();
        let output_size = if matches!(target, CaptureTarget::Display(_)) {
            PixelSize::new(
                (rect.max.x - rect.min.x) as u32,
                (rect.max.y - rect.min.y) as u32,
            )
        } else {
            size
        };
        config.setWidth(output_size.width as usize);
        config.setHeight(output_size.height as usize);
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        config.setColorSpaceName(kCGColorSpaceSRGB);
        config.setMinimumFrameInterval(CMTime::new(1, fps as i32));
        config.setShowsCursor(false);
        config.setCapturesAudio(false);
        config.setCaptureMicrophone(false);
        config.setQueueDepth(5);
        // SCStream.h documents this for display-bound windows/apps, not independent windows.
        // Set it explicitly; the independent-window filter's child behavior needs live validation.
        config.setIncludeChildWindows(true);
        config.setIgnoreShadowsSingleWindow(true);
        if matches!(target, CaptureTarget::Display(_)) && crop.is_some() {
            config.setSourceRect(CGRect::new(
                CGPoint::new(f64::from(rect.min.x) / scale, f64::from(rect.min.y) / scale),
                CGSize::new(
                    f64::from(rect.max.x - rect.min.x) / scale,
                    f64::from(rect.max.y - rect.min.y) / scale,
                ),
            ));
        }
        config
    };
    Ok(config)
}

impl Stream {
    pub fn new(
        content: &SCShareableContent,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        fps: u32,
        id: StreamId,
        shared: &Arc<Shared>,
    ) -> Result<Self, PlatformError> {
        let (filter, size, scale) = filter(content, target)?;
        let config = configuration(target, size, scale, crop, fps)?;
        let output = Output::new(shared, id);
        let queue = DispatchQueue::new("io.frostdev.crosspane.frames", None);
        // SAFETY: Filter/configuration are initialized and retained through construction. Output
        // implements both exact protocols, and the private queue is serial and retained by Self.
        let stream = unsafe {
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| {
                    PlatformError::Backend(format!(
                        "add SCK output: {} code {}",
                        e.domain(),
                        e.code()
                    ))
                })?;
            stream
        };
        Ok(Self {
            target,
            size,
            scale,
            crop,
            fps,
            stream,
            _output: output,
            _queue: queue,
        })
    }

    pub fn start(&mut self, deadline: Instant, wait: &Wait<'_>) -> Result<(), PlatformError> {
        let (tx, rx) = mpsc::channel();
        let stream = self.stream.clone();
        let callback = RcBlock::new(move |error: *mut NSError| {
            let result = error_result(error);
            if Instant::now() >= deadline {
                // SAFETY: Retained live SCK stream. A late start must be stopped even if the
                // worker already timed out and submitted a stop before this callback arrived.
                unsafe { stream.stopCaptureWithCompletionHandler(None) };
            }
            let _ = tx.send(result);
        });
        // SAFETY: Native stream is live and configured; SCK copies the completion block.
        unsafe {
            self.stream
                .startCaptureWithCompletionHandler(Some(&callback))
        };
        wait.check_result(
            wait.receive(&rx, deadline, Some(&self.stream))?,
            Some(&self.stream),
        )
    }

    pub fn stop_async(&self) -> mpsc::Receiver<Result<(), PlatformError>> {
        let (callback, rx) = completion();
        // SAFETY: Native stream is live; SCK copies the completion block. Never wait in callbacks.
        unsafe {
            self.stream
                .stopCaptureWithCompletionHandler(Some(&callback))
        };
        rx
    }

    pub fn set_crop(
        &mut self,
        crop: Option<PixelRect>,
        deadline: Instant,
        wait: &Wait<'_>,
    ) -> Result<(), PlatformError> {
        // Validate against fresh geometry and submit only the requested crop's configuration.
        let (size, scale) = match self.target {
            CaptureTarget::Display(id) if crop_needs_refresh(self.target, crop, self.size) => {
                display_geometry(id)?
            }
            _ => (self.size, self.scale),
        };
        self.update(size, scale, crop, deadline, wait)
    }

    pub fn resize(
        &mut self,
        content: &SCShareableContent,
        deadline: Instant,
        wait: &Wait<'_>,
    ) -> Result<(), PlatformError> {
        let (_, size, scale) = filter(content, self.target)?;
        if size != self.size || scale != self.scale {
            // Preserve only the old crop's intersection when the target shrinks. The engine can
            // supply its new crop next; never read outside the resized IOSurface.
            let crop = self.crop.and_then(|crop| {
                crop.intersection(&PixelRect::new(
                    [0, 0].into(),
                    [size.width as i32, size.height as i32].into(),
                ))
            });
            if self.crop.is_some() && crop.is_none() {
                return Err(PlatformError::Backend(
                    "crop no longer intersects target".into(),
                ));
            }
            self.update(size, scale, crop, deadline, wait)?;
        }
        Ok(())
    }

    fn update(
        &mut self,
        size: PixelSize,
        scale: f64,
        crop: Option<PixelRect>,
        deadline: Instant,
        wait: &Wait<'_>,
    ) -> Result<(), PlatformError> {
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        let config = configuration(self.target, size, scale, crop, self.fps)?;
        let (callback, rx) = completion();
        // SAFETY: Worker owns all configuration updates. SCK copies/retains arguments for the
        // async call; no mutex or AppKit main thread is held during its bounded wait.
        unsafe {
            self.stream
                .updateConfiguration_completionHandler(&config, Some(&callback))
        };
        wait.check_result(
            wait.receive(&rx, deadline, Some(&self.stream))?,
            Some(&self.stream),
        )?;
        self.size = size;
        self.scale = scale;
        self.crop = crop;
        Ok(())
    }

    pub fn cpu_crop(&self) -> Option<PixelRect> {
        if matches!(self.target, CaptureTarget::Window(_)) {
            self.crop
        } else {
            None
        }
    }

    pub fn cursor_crop(&self) -> Option<PixelRect> {
        self.crop
    }
}

type Completion = (
    RcBlock<dyn Fn(*mut NSError)>,
    mpsc::Receiver<Result<(), PlatformError>>,
);

fn completion() -> Completion {
    let (tx, rx) = mpsc::channel();
    (
        RcBlock::new(move |error: *mut NSError| {
            let _ = tx.send(error_result(error));
        }),
        rx,
    )
}

fn dirty_rect(rect: CGRect, scale: f64, region: PixelRect) -> Option<PixelRect> {
    let coords = [
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
        scale,
    ];
    if coords.iter().any(|c| !c.is_finite())
        || scale <= 0.0
        || rect.size.width <= 0.0
        || rect.size.height <= 0.0
    {
        return None;
    }
    let pixels = PixelRect::new(
        [
            (rect.origin.x * scale).floor() as i32,
            (rect.origin.y * scale).floor() as i32,
        ]
        .into(),
        [
            ((rect.origin.x + rect.size.width) * scale).ceil() as i32,
            ((rect.origin.y + rect.size.height) * scale).ceil() as i32,
        ]
        .into(),
    );
    let clipped = pixels.intersection(&region)?;
    Some(PixelRect::new(
        [clipped.min.x - region.min.x, clipped.min.y - region.min.y].into(),
        [clipped.max.x - region.min.x, clipped.max.y - region.min.y].into(),
    ))
}

pub(super) static HELD_BUFFERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub(crate) struct SckImage {
    pub(crate) buffer: CFRetained<CVPixelBuffer>,
    pub(crate) region: PixelRect,
    pub(crate) access: Mutex<()>,
}

// SAFETY: The retained completed SCK buffer is immutable pixel storage. CPU mappings and
// encoder submissions are serialized by access; its CF retain/release is thread-safe.
unsafe impl Send for SckImage {}
// SAFETY: Same immutable storage and serialized access contract as Send above.
unsafe impl Sync for SckImage {}

impl SckImage {
    pub(crate) fn new(buffer: CFRetained<CVPixelBuffer>, region: PixelRect) -> Self {
        HELD_BUFFERS.fetch_add(1, Ordering::Relaxed);
        Self {
            buffer,
            region,
            access: Mutex::new(()),
        }
    }
}

impl Drop for SckImage {
    fn drop(&mut self) {
        HELD_BUFFERS.fetch_sub(1, Ordering::Relaxed);
        // The CFRetained field releases its reference after this method returns.
    }
}

struct ReadLock<'a>(&'a CVPixelBuffer);

impl Drop for ReadLock<'_> {
    fn drop(&mut self) {
        // SAFETY: Balances the successful read-only lock, also if the reader panics.
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly) };
    }
}

impl NativeImage for SckImage {
    fn size(&self) -> PixelSize {
        PixelSize::new(
            (self.region.max.x - self.region.min.x) as u32,
            (self.region.max.y - self.region.min.y) as u32,
        )
    }

    fn read(&self, f: &mut dyn FnMut(&[u8], u32)) -> Result<(), PlatformError> {
        let _access = self.access.lock().unwrap_or_else(|e| e.into_inner());
        let buffer = &self.buffer;
        let invalid = || PlatformError::Unsupported("unmappable SCK BGRA buffer");
        let full = PixelSize::new(
            u32::try_from(CVPixelBufferGetWidth(buffer)).map_err(|_| invalid())?,
            u32::try_from(CVPixelBufferGetHeight(buffer)).map_err(|_| invalid())?,
        );
        checked_crop(Some(self.region), full)?;
        if CVPixelBufferGetPixelFormatType(buffer) != kCVPixelFormatType_32BGRA {
            return Err(invalid());
        }
        // SAFETY: Live BGRA buffer, read-only mapping serialized by access.
        if unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) } != 0 {
            return Err(invalid());
        }
        let _lock = ReadLock(buffer);
        let stride = CVPixelBufferGetBytesPerRow(buffer);
        let len = CVPixelBufferGetDataSize(buffer);
        let base = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
        let offset = (self.region.min.y as usize)
            .checked_mul(stride)
            .and_then(|n| n.checked_add(self.region.min.x as usize * 4))
            .ok_or_else(invalid)?;
        let needed = (self.size().height as usize - 1)
            .checked_mul(stride)
            .and_then(|n| n.checked_add(self.size().width as usize * 4))
            .ok_or_else(invalid)?;
        if base.is_null()
            || stride < full.width as usize * 4
            || len > isize::MAX as usize
            || offset.checked_add(needed).is_none_or(|end| end > len)
        {
            return Err(invalid());
        }
        let stride = u32::try_from(stride).map_err(|_| invalid())?;
        // SAFETY: The crop extent is checked against the locked allocation. The slice contains
        // only the required strided rows and cannot escape the callback's mapping lifetime.
        f(
            unsafe { std::slice::from_raw_parts(base.add(offset), needed) },
            stride,
        );
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub(crate) struct CaptureInput(pub(crate) Arc<dyn NativeImage>);

impl NativeInput for CaptureInput {
    fn size(&self) -> PixelSize {
        self.0.size()
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

#[cfg(test)]
fn copy_rows(
    source: &[u8],
    source_stride: usize,
    size: PixelSize,
    region: PixelRect,
) -> Option<(u32, Arc<[u8]>)> {
    checked_crop(Some(region), size).ok()?;
    let full_row = (size.width as usize).checked_mul(4)?;
    let stride = u32::try_from(region.max.x - region.min.x)
        .ok()?
        .checked_mul(4)?;
    let rows = usize::try_from(region.max.y - region.min.y).ok()?;
    if source_stride < full_row || source.len() < source_stride.checked_mul(size.height as usize)? {
        return None;
    }
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact((stride as usize).checked_mul(rows)?)
        .ok()?;
    for y in region.min.y..region.max.y {
        let start = (y as usize)
            .checked_mul(source_stride)?
            .checked_add((region.min.x as usize).checked_mul(4)?)?;
        pixels.extend_from_slice(source.get(start..start.checked_add(stride as usize)?)?);
    }
    Some((stride, pixels.into()))
}

pub(super) fn native_sample(
    sample: &CMSampleBuffer,
    scale: f64,
    crop: Option<PixelRect>,
) -> Option<Frame> {
    // SAFETY: The sample and its attachments/image buffer remain live throughout this callback.
    // We retain the attachment array and image buffer; no mutation of attachments takes place.
    let (attachments, image) = unsafe {
        (
            sample.sample_attachments_array(false)?,
            sample.image_buffer()?,
        )
    };
    // SAFETY: CoreMedia specifies this array's elements as CFMutableDictionary (CFType) objects.
    let attachments = unsafe { &*ptr::from_ref(&*attachments).cast::<CFArray<CFType>>() };
    let first = attachments.get(0)?;
    let dictionary = first.downcast_ref::<CFDictionary>()?;
    // SAFETY: SCK's sample attachments are dictionaries with CF object keys and values.
    let dictionary = unsafe { &*ptr::from_ref(dictionary).cast::<CFDictionary<CFType, CFType>>() };
    // SAFETY: SCK exports immutable NSString constants, toll-free bridged to CFString.
    let (status_key, dirty_key) = unsafe {
        (
            key(SCStreamFrameInfoStatus),
            key(SCStreamFrameInfoDirtyRects),
        )
    };
    let status = dictionary
        .get(status_key)?
        .downcast_ref::<CFNumber>()?
        .as_isize()?;
    if status != SCFrameStatus::Complete.0
        || CVPixelBufferGetPixelFormatType(&image) != kCVPixelFormatType_32BGRA
    {
        return None;
    }
    let size = PixelSize::new(
        u32::try_from(CVPixelBufferGetWidth(&image)).ok()?,
        u32::try_from(CVPixelBufferGetHeight(&image)).ok()?,
    );
    let region = checked_crop(crop, size).ok()?;
    let damage = dictionary.get(dirty_key).and_then(|value| {
        let rects = value.downcast_ref::<CFArray>()?;
        // SAFETY: SCK's dirtyRects array contains CF dictionary objects.
        let rects = unsafe { &*ptr::from_ref(rects).cast::<CFArray<CFType>>() };
        let mut damage = Vec::new();
        for i in 0..rects.len() {
            let value = rects.get(i)?;
            let dict = value.downcast_ref::<CFDictionary>()?;
            let mut rect = CGRect::ZERO;
            // SAFETY: SCK's dirtyRects contains CGRect dictionaries, dynamically type-checked
            // above. Output points to a writable CGRect; the API validates its numeric fields.
            if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(dict), &mut rect) } {
                return None;
            }
            if let Some(rect) = dirty_rect(rect, scale, region) {
                damage.push(rect);
            }
        }
        Some(damage)
    });
    // SAFETY: The sample remains live for the entire callback; this only reads its timestamp.
    let time = unsafe { sample.presentation_time_stamp() };
    let at = if time.flags.contains(CMTimeFlags::Valid)
        && !time.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
        && time.value >= 0
        && time.timescale > 0
        && time.epoch == 0
    {
        // SAFETY: A numeric SCK presentation time on CoreMedia's host clock; this public API
        // converts its timescale to mach absolute ticks, then the shared node clock converts it.
        clock::from_ticks(unsafe { CMClock::convert_host_time_to_system_units(time) })
    } else {
        clock::now()
    };
    let image = Arc::new(SckImage::new(image, region));
    Some(Frame {
        size: image.size(),
        image: FrameImage::Native(image),
        damage,
        at,
    })
}

fn key(string: &NSString) -> &objc2_core_foundation::CFType {
    let cf: &CFString = string.as_ref();
    cf.as_ref()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn native_crop_maps_in_place_packs_and_releases() {
        use objc2_core_video::CVPixelBufferCreate;
        use std::ptr::NonNull;
        let baseline = super::super::held_capture_buffers();
        let mut raw = ptr::null_mut();
        // SAFETY: Valid BGRA dimensions and writable output pointer.
        assert_eq!(
            // SAFETY: Valid dimensions and writable output; ownership transferred on success.
            unsafe {
                CVPixelBufferCreate(
                    None,
                    4,
                    3,
                    kCVPixelFormatType_32BGRA,
                    None,
                    NonNull::from(&mut raw),
                )
            },
            0
        );
        // SAFETY: Successful Create transfers its +1 reference.
        let buffer = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };
        // SAFETY: Exclusively owned test buffer, balanced write lock/unlock.
        assert_eq!(
            // SAFETY: Exclusively owned buffer; balanced lock/unlock below.
            unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) },
            0
        );
        let stride = CVPixelBufferGetBytesPerRow(&buffer);
        let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
        // SAFETY: Initialize the entire live locked allocation, then write in-bounds pixels.
        unsafe {
            ptr::write_bytes(base, 0, CVPixelBufferGetDataSize(&buffer));
            for y in 0..3 {
                for x in 0..16 {
                    *base.add(y * stride + x) = (y * 16 + x) as u8;
                }
            }
            assert_eq!(
                CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()),
                0
            );
        }
        let image: Arc<dyn NativeImage> = Arc::new(SckImage::new(
            buffer,
            PixelRect::new([1, 1].into(), [3, 3].into()),
        ));
        assert_eq!(super::super::held_capture_buffers(), baseline + 1);
        let input = super::super::capture_input(&image).unwrap();
        let frame = Frame {
            size: image.size(),
            image: FrameImage::Native(image),
            damage: None,
            at: crosspane_types::time::MonoTime::ZERO,
        };
        frame
            .with_pixels(|pixels, row| {
                assert_eq!(row as usize, stride);
                // SAFETY: In-bounds pointer arithmetic in the same retained allocation.
                assert_eq!(pixels.as_ptr(), unsafe { base.add(stride + 4) });
                assert_eq!(&pixels[..8], &[20, 21, 22, 23, 24, 25, 26, 27]);
                assert_eq!(
                    &pixels[stride..stride + 8],
                    &[36, 37, 38, 39, 40, 41, 42, 43]
                );
            })
            .unwrap();
        let (packed, row) = frame.to_cpu().unwrap();
        assert_eq!(row, 8);
        assert_eq!(
            &*packed,
            &[
                20, 21, 22, 23, 24, 25, 26, 27, 36, 37, 38, 39, 40, 41, 42, 43
            ]
        );
        drop(frame);
        assert_eq!(super::super::held_capture_buffers(), baseline + 1);
        drop(input);
        assert_eq!(super::super::held_capture_buffers(), baseline);
    }

    #[test]
    fn crop_refresh_only_for_displays_outside_cached_size() {
        let display = CaptureTarget::Display(DisplayId(1));
        let window = CaptureTarget::Window(crosspane_types::id::WindowId(1));
        let size = PixelSize::new(100, 80);
        let fits = PixelRect::new([10, 10].into(), [100, 80].into());
        let wider = PixelRect::new([10, 10].into(), [101, 80].into());
        let taller = PixelRect::new([10, 10].into(), [100, 81].into());
        assert!(!crop_needs_refresh(display, None, size));
        assert!(!crop_needs_refresh(display, Some(fits), size));
        assert!(crop_needs_refresh(display, Some(wider), size));
        assert!(crop_needs_refresh(display, Some(taller), size));
        assert!(!crop_needs_refresh(window, Some(wider), size));
        assert!(!crop_needs_refresh(window, Some(taller), size));
    }

    #[test]
    fn refreshed_size_accepts_only_nonempty_in_bounds_crops() {
        let cached = PixelSize::new(100, 80);
        let current = PixelSize::new(200, 160);
        let crop = PixelRect::new([10, 20].into(), [200, 160].into());
        assert!(checked_crop(Some(crop), cached).is_err());
        assert_eq!(checked_crop(Some(crop), current).unwrap(), crop);
        for invalid in [
            PixelRect::new([10, 20].into(), [201, 160].into()),
            PixelRect::new([10, 20].into(), [200, 161].into()),
            PixelRect::new([-1, 0].into(), [100, 80].into()),
            PixelRect::new([10, 20].into(), [10, 20].into()),
            PixelRect::new([20, 20].into(), [10, 10].into()),
        ] {
            assert!(checked_crop(Some(invalid), current).is_err());
        }
    }

    #[test]
    fn live_main_display_crop_refresh_continues_frames() {
        use std::sync::atomic::AtomicBool;
        use std::sync::{Condvar, Mutex};
        use std::time::Duration;

        use crosspane_platform::{FrameEvent, IoGate};
        use objc2_core_graphics::{
            CGColorSpace, CGDisplayCopyColorSpace, CGDisplayModelNumber, CGDisplayVendorNumber,
            CGMainDisplayID,
        };

        if std::env::var("CROSSPANE_MAC_LIVE").as_deref() != Ok("1") {
            eprintln!(
                "skipped: display crop refresh requires CROSSPANE_MAC_LIVE=1 in the GUI session"
            );
            return;
        }
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let (commands, _receiver) = mpsc::channel();
        let shared = Arc::new(Shared {
            gate,
            deliveries: Mutex::new(Default::default()),
            commands,
            cursor_wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let wait = Wait::new(&shared, std::iter::empty());
        let content = content(Instant::now() + super::super::TIMEOUT, &wait).unwrap();
        // SAFETY: Read-only display IDs from the retained, immutable SCK snapshot.
        for display in unsafe { content.0.displays() } {
            // SAFETY: Read-only ID of a retained display from the snapshot.
            let id = unsafe { display.displayID() };
            if CGDisplayVendorNumber(id) == crate::displays::TWIN_VENDOR
                && CGDisplayModelNumber(id) == crate::displays::TWIN_PRODUCT
            {
                let space = CGDisplayCopyColorSpace(id);
                eprintln!(
                    "twin display {id} colour-space name: {:?}",
                    CGColorSpace::name(Some(&space)).map(|name| name.to_string())
                );
            }
        }
        let target = CaptureTarget::Display(DisplayId(CGMainDisplayID()));
        let (size, scale) = display_geometry(DisplayId(CGMainDisplayID())).unwrap();
        assert!(size.width >= 4 && size.height >= 4);
        let cached = PixelSize::new(size.width / 4, size.height / 4);
        let initial = checked_crop(None, cached).unwrap();
        let config = configuration(target, size, scale, Some(initial), 30).unwrap();
        // SAFETY: Read-only property of the live configuration; the sRGB constant is immutable.
        unsafe { assert_eq!(&*config.colorSpaceName(), kCGColorSpaceSRGB) };
        let enlarged = PixelRect::new(
            [0, 0].into(),
            [size.width as i32 / 2, size.height as i32 / 2].into(),
        );
        let (tx, rx) = mpsc::channel();
        let id = StreamId(1);
        let mut stream = Stream::new(&content.0, target, Some(initial), 30, id, &shared).unwrap();
        shared.deliveries.lock().unwrap().insert(
            id,
            super::super::Delivery {
                sink: Arc::new(move |event| {
                    let _ = tx.send(event);
                }),
                active: true,
                scale,
                crop: None,
                interval: Duration::from_nanos(1_000_000_000_u64.div_ceil(30)),
                last: None,
                size: None,
                full_damage: true,
                cursor: super::super::cursor::StreamCursor::new(target, Some(initial)),
            },
        );
        let await_frame = |expected| {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap()
                {
                    FrameEvent::Frame { stream, frame } => {
                        assert_eq!(stream, id);
                        if frame.size == expected {
                            let (pixels, stride) = frame.to_cpu().unwrap();
                            assert_eq!(stride, expected.width * 4);
                            assert_eq!(pixels.len(), stride as usize * expected.height as usize);
                            break;
                        }
                    }
                    FrameEvent::Ended { reason, .. } => panic!("capture ended: {reason:?}"),
                    _ => {}
                }
            }
        };
        stream
            .start(Instant::now() + super::super::TIMEOUT, &wait)
            .unwrap();
        await_frame(cached);
        // Only the cache changes; the real display mode and owner's windows stay untouched.
        stream.size = cached;
        stream.scale = scale / 2.0;
        assert!(crop_needs_refresh(target, Some(enlarged), stream.size));
        stream
            .set_crop(
                Some(enlarged),
                Instant::now() + super::super::TIMEOUT,
                &wait,
            )
            .unwrap();
        assert_eq!(stream.size, size);
        assert_eq!(stream.scale, scale);
        await_frame(PixelSize::new(size.width / 2, size.height / 2));
        wait.receive(
            &stream.stop_async(),
            Instant::now() + super::super::TIMEOUT,
            None,
        )
        .unwrap()
        .unwrap();
    }

    #[test]
    fn dirty_rect_points_to_pixels_at_scale_two() {
        let region = PixelRect::new([0, 0].into(), [40, 40].into());
        let rect = CGRect::new(CGPoint::new(1.25, 2.5), CGSize::new(3.5, 4.25));
        assert_eq!(
            dirty_rect(rect, 2.0, region),
            Some(PixelRect::new([2, 5].into(), [10, 14].into()))
        );
        let crop = PixelRect::new([4, 6].into(), [8, 12].into());
        assert_eq!(
            dirty_rect(rect, 2.0, crop),
            Some(PixelRect::new([0, 0].into(), [4, 6].into()))
        );
        assert!(dirty_rect(CGRect::ZERO, 2.0, region).is_none());
        assert!(dirty_rect(rect, f64::NAN, region).is_none());
    }

    #[test]
    fn bgra_rows_discard_padding() {
        let source = [
            1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16, 88, 88, 88, 88,
        ];
        let size = PixelSize::new(2, 2);
        let bounds = PixelRect::new([0, 0].into(), [2, 2].into());
        let (stride, pixels) = copy_rows(&source, 12, size, bounds).unwrap();
        assert_eq!(stride, 8);
        assert_eq!(
            &*pixels,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
        let (_, pixels) = copy_rows(
            &source,
            12,
            size,
            PixelRect::new([1, 0].into(), [2, 2].into()),
        )
        .unwrap();
        assert_eq!(&*pixels, &[5, 6, 7, 8, 13, 14, 15, 16]);
        assert!(copy_rows(&source[..20], 12, size, bounds).is_none());
        assert!(copy_rows(&source, 4, size, bounds).is_none());
    }
}
