//! E2 capture of window pixels (03 §4.2). Frozen by WP-2.1 (docs/wp/E2-v0.md); native frames by
//! WP-2.24 (docs/wp/GPU-v0.md).
//!
//! A frame is in CPU memory (Hyprland SHM) or stays where the OS put it (an IOSurface on macOS, a
//! DMA-BUF on Linux) so the GPU can read it without a copy.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crosspane_types::geom::{PixelRect, PixelSize};
use crosspane_types::id::{DisplayId, WindowId};
use crosspane_types::time::MonoTime;

use crate::{EventSink, PlatformError};

/// What to capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CaptureTarget {
    /// A whole display (Hyprland M2: the twin output).
    Display(DisplayId),
    /// One window with its child windows (macOS M1).
    Window(WindowId),
}

/// Identifies one capture stream on this node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamId(pub u64);

/// One captured image: 8-bit **BGRA** (alpha ignored), top row first.
#[derive(Clone, Debug)]
pub struct Frame {
    pub size: PixelSize,
    pub image: FrameImage,
    /// What changed since the previous frame of this stream, if the OS reported it. `None` means
    /// unknown: treat the whole frame as damaged.
    pub damage: Option<Vec<PixelRect>>,
    /// Capture time on this node's monotonic clock.
    pub at: MonoTime,
}

/// Where a frame's pixels are.
#[derive(Clone, Debug)]
pub enum FrameImage {
    /// Rows of `stride` bytes in CPU memory.
    Cpu { stride: u32, pixels: Arc<[u8]> },
    /// An image the backend owns (an IOSurface-backed `CVPixelBuffer`, a DMA-BUF that is also a
    /// GPU texture). Dropping the last `Arc` returns the buffer to the backend's capture ring, so
    /// holders keep at most two per stream: the frame being encoded and the last one.
    Native(Arc<dyn NativeImage>),
}

/// A captured image in native memory. The concrete type behind [`NativeImage::as_any`] is private
/// to the platform crate that produced it, which also provides its GPU import.
pub trait NativeImage: Send + Sync + fmt::Debug {
    fn size(&self) -> PixelSize;
    /// Map the image for CPU reading and call `f` once with BGRA8 rows of `stride` bytes (the
    /// slice covers at least `(size.height - 1) * stride + size.width * 4` bytes). The mapping
    /// ends when `f` returns. `Err(Unsupported)` when the image can't be mapped.
    fn read(&self, f: &mut dyn FnMut(&[u8], u32)) -> Result<(), PlatformError>;
    fn as_any(&self) -> &dyn Any;
}

impl Frame {
    /// A frame in CPU memory.
    pub fn cpu(
        size: PixelSize,
        stride: u32,
        pixels: Arc<[u8]>,
        damage: Option<Vec<PixelRect>>,
        at: MonoTime,
    ) -> Frame {
        Frame {
            size,
            image: FrameImage::Cpu { stride, pixels },
            damage,
            at,
        }
    }

    /// The rows and stride when the pixels are in CPU memory.
    pub fn cpu_pixels(&self) -> Option<(&[u8], u32)> {
        match &self.image {
            FrameImage::Cpu { stride, pixels } => Some((pixels, *stride)),
            FrameImage::Native(_) => None,
        }
    }

    /// The native image, when the pixels aren't in CPU memory.
    pub fn native(&self) -> Option<&Arc<dyn NativeImage>> {
        match &self.image {
            FrameImage::Cpu { .. } => None,
            FrameImage::Native(image) => Some(image),
        }
    }

    /// Run `f` over the rows and stride wherever the pixels are (a native image is mapped for the
    /// duration of the call).
    pub fn with_pixels<R>(&self, f: impl FnOnce(&[u8], u32) -> R) -> Result<R, PlatformError> {
        match &self.image {
            FrameImage::Cpu { stride, pixels } => Ok(f(pixels, *stride)),
            FrameImage::Native(image) => {
                let mut f = Some(f);
                let mut result = None;
                image.read(&mut |pixels, stride| {
                    if let Some(f) = f.take() {
                        result = Some(f(pixels, stride));
                    }
                })?;
                result.ok_or(PlatformError::Backend(
                    "native image read didn't call back".into(),
                ))
            }
        }
    }

    /// The pixels in CPU memory, copied out of a native image as tightly packed rows.
    pub fn to_cpu(&self) -> Result<(Arc<[u8]>, u32), PlatformError> {
        if let FrameImage::Cpu { stride, pixels } = &self.image {
            return Ok((Arc::clone(pixels), *stride));
        }
        let (width, height) = (self.size.width as usize, self.size.height as usize);
        self.with_pixels(|pixels, stride| {
            let mut packed = Vec::with_capacity(width * 4 * height);
            for row in pixels.chunks(stride as usize).take(height) {
                packed.extend_from_slice(row.get(..width * 4)?);
            }
            (packed.len() == width * 4 * height).then_some(packed)
        })?
        .map(|packed| (Arc::from(packed), self.size.width * 4))
        .ok_or(PlatformError::Backend(
            "native image smaller than its size".into(),
        ))
    }
}

/// The cursor the pointer shows over captured content (03 §4.6, WP-2.16): 8-bit **BGRA** with
/// straight (not premultiplied) alpha, rows of `size.width * 4` bytes, top row first, at the
/// captured content's pixel density. `hotspot` is the click point in pixels from the top-left
/// corner, inside the image. Each side is at most 256 pixels (backends scale a larger cursor
/// down).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    pub size: PixelSize,
    pub hotspot: (u32, u32),
    pub pixels: Arc<[u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamEndReason {
    Requested,
    /// The target went away (window closed, display removed).
    TargetGone,
    /// The I/O gate closed (lock, sleep, panic; 04 §7) or permission was revoked.
    Blocked,
    Failed,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum FrameEvent {
    Frame {
        stream: StreamId,
        frame: Frame,
    },
    Ended {
        stream: StreamId,
        reason: StreamEndReason,
    },
    /// The cursor shape over the captured content changed (WP-2.16). Sent when the pointer is over
    /// the content and its shape differs from the last one this stream reported (so at least
    /// once, the first time the pointer is over it); `None` when the app hid the cursor. Never
    /// sent while the pointer is elsewhere. A backend that can't observe the cursor never sends
    /// it, and the destination keeps its default cursor.
    Cursor {
        stream: StreamId,
        cursor: Option<CursorImage>,
    },
    /// The pointer is over the captured content but the backend can't see its shape right now
    /// (e.g. a compositor-drawn themed cursor that its cursor capture doesn't render): the
    /// destination shows its own default cursor. Sent on the same change rules as `Cursor`.
    CursorDefault {
        stream: StreamId,
    },
}

/// Captures pixels for projection.
///
/// - **Gate:** backends receive the node's `IoGate` at construction. While it is closed they
///   deliver no frames, and they end every stream with `Blocked` when it closes (never capture a
///   lock screen, 04 §7).
/// - Frames are delivered at most `max_fps` per second; a backend may skip frames when the sink is
///   behind (only the newest matters).
pub trait FrameCapture: Send {
    /// Start capturing `target`, cropped to `crop` (device pixels on the target) if given.
    fn start(
        &mut self,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError>;

    /// Change a running stream's crop (after a resize).
    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError>;

    /// Stop a stream; emits `Ended { Requested }`.
    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3×2 image with 16-byte rows, readable unless `mappable` is false.
    #[derive(Debug)]
    struct Fake {
        mappable: bool,
    }

    const ROWS: [u8; 32] = {
        let mut rows = [0xEE; 32];
        let mut i = 0;
        while i < 12 {
            rows[i] = i as u8;
            rows[16 + i] = 100 + i as u8;
            i += 1;
        }
        rows
    };

    impl NativeImage for Fake {
        fn size(&self) -> PixelSize {
            PixelSize::new(3, 2)
        }
        fn read(&self, f: &mut dyn FnMut(&[u8], u32)) -> Result<(), PlatformError> {
            if !self.mappable {
                return Err(PlatformError::Unsupported("device memory"));
            }
            f(&ROWS[..28], 16);
            Ok(())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn native(mappable: bool) -> Frame {
        Frame {
            size: PixelSize::new(3, 2),
            image: FrameImage::Native(Arc::new(Fake { mappable })),
            damage: None,
            at: MonoTime::ZERO,
        }
    }

    #[test]
    fn native_frames_read_in_place_and_pack_on_copy() {
        let frame = native(true);
        assert!(frame.cpu_pixels().is_none());
        assert!(frame.native().is_some());
        assert_eq!(
            frame
                .with_pixels(|pixels, stride| (pixels[16], stride))
                .ok(),
            Some((100, 16))
        );
        let (packed, stride) = frame.to_cpu().ok().unwrap_or_default();
        assert_eq!(stride, 12);
        let expected: Vec<u8> = (0..12).chain(100..112).collect();
        assert_eq!(&packed[..], &expected[..]);
        assert!(native(false).with_pixels(|_, _| ()).is_err());
        assert!(native(false).to_cpu().is_err());
    }

    #[test]
    fn cpu_frames_are_shared_not_copied() {
        let pixels: Arc<[u8]> = Arc::from(&ROWS[..]);
        let frame = Frame::cpu(
            PixelSize::new(3, 2),
            16,
            Arc::clone(&pixels),
            None,
            MonoTime::ZERO,
        );
        assert_eq!(frame.cpu_pixels().map(|(_, stride)| stride), Some(16));
        let (shared, stride) = frame.to_cpu().ok().unwrap_or_default();
        assert!(Arc::ptr_eq(&shared, &pixels));
        assert_eq!(stride, 16);
    }
}
