//! Cursor observation is independent of SCK completion waits. Only bitmap/source reads use
//! AppKit, on the main thread; the watcher and its cache contain only Rust values.

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crosspane_platform::{CaptureTarget, CursorImage, FrameEvent, PlatformError};
use crosspane_types::geom::{PixelRect, PixelSize};
use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker, msg_send};
use objc2_app_kit::{
    NSBitmapFormat, NSBitmapImageRep, NSCompositingOperation, NSCursor, NSDeviceRGBColorSpace,
    NSGraphicsContext, NSImage,
};
use objc2_core_foundation::{CFDictionary, CFNumber, CFString, CFType, CGPoint, CGRect, CGSize};
#[allow(deprecated)] // Public, still exported by CoreGraphics; no CGS/SLS cursor APIs.
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo,
    CGWindowListOption, kCGWindowBounds, kCGWindowNumber,
};

use super::Shared;
use crate::main_thread::on_main;

const TICK: Duration = Duration::from_millis(50);
const MAIN_TIMEOUT: Duration = Duration::from_millis(30);
const BOUNDS_TICK: Duration = Duration::from_millis(250);
const ERROR_INTERVAL: Duration = Duration::from_secs(10);

pub(super) struct StreamCursor {
    target: CaptureTarget,
    pub crop: Option<PixelRect>,
    bounds: Option<(Instant, Option<CGRect>)>,
    last: Option<u64>,
}

impl StreamCursor {
    pub fn new(target: CaptureTarget, crop: Option<PixelRect>) -> Self {
        Self {
            target,
            crop,
            bounds: None,
            last: None,
        }
    }

    fn bounds(&mut self) -> Result<CGRect, &'static str> {
        match self.target {
            CaptureTarget::Display(id) => Ok(CGDisplayBounds(id.0)),
            CaptureTarget::Window(id) => {
                let now = Instant::now();
                if self
                    .bounds
                    .is_none_or(|(at, _)| now.duration_since(at) >= BOUNDS_TICK)
                {
                    // Cache failures too: an unavailable window must not be queried every tick.
                    self.bounds = Some((now, window_bounds(id.0)));
                }
                self.bounds
                    .and_then(|(_, bounds)| bounds)
                    .ok_or("window bounds unavailable")
            }
            _ => Err("unknown cursor capture target"),
        }
    }

    fn changed(&mut self, cursor: &Option<CursorImage>) -> bool {
        let mut hash = DefaultHasher::new();
        cursor.is_some().hash(&mut hash);
        if let Some(image) = cursor {
            image.size.hash(&mut hash);
            image.hotspot.hash(&mut hash);
            image.pixels.hash(&mut hash);
        }
        let hash = hash.finish();
        if self.last == Some(hash) {
            false
        } else {
            self.last = Some(hash);
            true
        }
    }
}

fn window_bounds(id: u64) -> Option<CGRect> {
    let id = u32::try_from(id).ok()?;
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionIncludingWindow, id)?;
    // SAFETY: Quartz returns CF dictionaries in this array; concrete types are checked below.
    let list = unsafe { list.cast_unchecked::<CFType>() };
    // SAFETY: Immutable, public CFString constants exported by CoreGraphics.
    let (number, bounds) = unsafe { (kCGWindowNumber, kCGWindowBounds) };
    for value in list.iter() {
        let dictionary = value.downcast::<CFDictionary>().ok()?;
        // SAFETY: Quartz window dictionaries have CFString keys and CF object values.
        let dictionary = unsafe { dictionary.cast_unchecked::<CFString, CFType>() };
        if dictionary
            .get(number)?
            .downcast::<CFNumber>()
            .ok()?
            .as_i64()?
            != i64::from(id)
        {
            continue;
        }
        let bounds = dictionary.get(bounds)?.downcast::<CFDictionary>().ok()?;
        let mut rect = CGRect::ZERO;
        // SAFETY: Runtime-checked bounds dictionary and valid writable CGRect storage.
        return unsafe { CGRectMakeWithDictionaryRepresentation(Some(&bounds), &mut rect) }
            .then_some(rect);
    }
    None
}

fn point_in_content(point: CGPoint, bounds: CGRect, crop: Option<PixelRect>, scale: f64) -> bool {
    if [
        point.x,
        point.y,
        bounds.origin.x,
        bounds.origin.y,
        bounds.size.width,
        bounds.size.height,
        scale,
    ]
    .iter()
    .any(|n| !n.is_finite())
        || scale <= 0.0
        || bounds.size.width <= 0.0
        || bounds.size.height <= 0.0
    {
        return false;
    }
    let x = point.x - bounds.origin.x;
    let y = point.y - bounds.origin.y;
    x >= 0.0
        && y >= 0.0
        && x < bounds.size.width
        && y < bounds.size.height
        && crop.is_none_or(|crop| {
            !crop.is_empty()
                && x >= f64::from(crop.min.x) / scale
                && y >= f64::from(crop.min.y) / scale
                && x < f64::from(crop.max.x) / scale
                && y < f64::from(crop.max.y) / scale
        })
}

fn geometry(points: CGSize, hotspot: CGPoint, scale: f64) -> Option<(PixelSize, (u32, u32))> {
    if [points.width, points.height, hotspot.x, hotspot.y, scale]
        .iter()
        .any(|n| !n.is_finite())
        || points.width <= 0.0
        || points.height <= 0.0
        || scale <= 0.0
    {
        return None;
    }
    let width = (points.width * scale).round();
    let height = (points.height * scale).round();
    if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
        return None;
    }
    let fit = (256.0 / width.max(height)).min(1.0);
    let size = PixelSize::new(
        (width * fit).round().clamp(1.0, 256.0) as u32,
        (height * fit).round().clamp(1.0, 256.0) as u32,
    );
    Some((
        size,
        (
            (hotspot.x * scale * fit)
                .round()
                .clamp(0.0, f64::from(size.width - 1)) as u32,
            (hotspot.y * scale * fit)
                .round()
                .clamp(0.0, f64::from(size.height - 1)) as u32,
        ),
    ))
}

fn unpremultiply(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = (u32::from(*channel) * 255 + alpha / 2)
                .checked_div(alpha)
                .unwrap_or(0)
                .min(255) as u8;
        }
    }
}

fn hidden(pixels: &[u8]) -> bool {
    !pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[3] == 255)
}

fn render(
    image: &NSImage,
    points: CGSize,
    hotspot: CGPoint,
    scale: f64,
) -> Result<Option<CursorImage>, &'static str> {
    let (size, hotspot) = geometry(points, hotspot, scale).ok_or("invalid cursor geometry")?;
    let stride = size.width as usize * 4;
    let len = stride * size.height as usize;
    // SAFETY: Null planes request bitmap-owned storage. Dimensions are 1..=256, with explicit
    // 8-bit, four-channel, interleaved premultiplied RGBA and a checked, tightly packed stride.
    let bitmap = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(), ptr::null_mut(), size.width as isize,
            size.height as isize, 8, 4, true, false, NSDeviceRGBColorSpace,
            NSBitmapFormat::empty(), stride as isize, 32,
        )
    }.ok_or("cursor bitmap allocation failed")?;
    let data = bitmap.bitmapData();
    if data.is_null() || bitmap.bytesPerRow() != stride as isize {
        return Err("cursor bitmap storage unavailable");
    }
    // SAFETY: The new retained bitmap owns len writable bytes with the verified stride. No
    // slice survives the drawing operation, and no other thread can access this bitmap.
    unsafe { std::slice::from_raw_parts_mut(data, len) }.fill(0);
    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap)
        .ok_or("cursor graphics context unavailable")?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    image.drawInRect_fromRect_operation_fraction(
        CGRect::new(
            CGPoint::ZERO,
            CGSize::new(f64::from(size.width), f64::from(size.height)),
        ),
        CGRect::ZERO,
        NSCompositingOperation::Copy,
        1.0,
    );
    context.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();
    // SAFETY: Drawing has finished; bitmap still owns len readable bytes, exclusively on main.
    // NSBitmapImageRep storage is top-row-first; its nonflipped context handles image orientation.
    let mut pixels = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2); // RGBA -> BGRA.
    }
    unpremultiply(&mut pixels);
    Ok((!hidden(&pixels)).then(|| CursorImage {
        size,
        hotspot,
        pixels: pixels.into(),
    }))
}

#[derive(Default)]
struct Cache {
    source: Option<u64>,
    images: BTreeMap<u64, Option<CursorImage>>,
}

#[allow(deprecated)] // The WP explicitly requires the public system-cursor API, not currentCursor.
fn read(
    _mtm: MainThreadMarker,
    scales: &[u64],
    cache: &mut Cache,
) -> Result<BTreeMap<u64, Option<CursorImage>>, &'static str> {
    // Not CGCursorIsVisible: apps hide the cursor while the user types until the mouse moves, and
    // motion injected into a projected window doesn't end that, so shapes would stop updating.
    // A cursor counts as hidden only when its image has no opaque pixel.
    let cursor = NSCursor::currentSystemCursor().ok_or("nil system cursor")?;
    // SAFETY: NSCursor's public image getter returns an NSImage. Use a nullable retained return
    // to handle an unexpected nil without a panic; all AppKit access stays on the main thread.
    let image: Option<Retained<NSImage>> = unsafe { msg_send![&cursor, image] };
    let image = image.ok_or("nil cursor image")?;
    let points = image.size();
    let hotspot = cursor.hotSpot();
    let tiff = image
        .TIFFRepresentation()
        .ok_or("cursor TIFF unavailable")?;
    let mut source = DefaultHasher::new();
    // SAFETY: The retained, immutable TIFF NSData is read only on main and never mutated.
    unsafe { tiff.as_bytes_unchecked() }.hash(&mut source);
    for n in [points.width, points.height, hotspot.x, hotspot.y] {
        n.to_bits().hash(&mut source);
    }
    let source = source.finish();
    if cache.source != Some(source) {
        cache.source = Some(source);
        cache.images.clear();
    }
    cache.images.retain(|scale, _| scales.contains(scale));
    for &scale in scales {
        if let std::collections::btree_map::Entry::Vacant(entry) = cache.images.entry(scale) {
            entry.insert(render(&image, points, hotspot, f64::from_bits(scale))?);
        }
    }
    Ok(cache.images.clone())
}

fn tick(
    shared: &Arc<Shared>,
    cache: &Arc<Mutex<Cache>>,
    pending: &Arc<AtomicBool>,
) -> Result<(), &'static str> {
    if !shared.permitted() {
        return Ok(());
    }
    let event = CGEvent::new(None).ok_or("pointer event unavailable")?;
    let point = CGEvent::location(Some(&event));
    let mut over = Vec::new();
    {
        let mut deliveries = shared.deliveries.lock().unwrap_or_else(|e| e.into_inner());
        for (&id, delivery) in deliveries.iter_mut().filter(|(_, d)| d.active) {
            let bounds = delivery.cursor.bounds()?;
            if point_in_content(point, bounds, delivery.cursor.crop, delivery.scale) {
                over.push((id, delivery.scale.to_bits(), delivery.cursor.crop));
            } else {
                delivery.cursor.last = None; // Re-entry always reports, even if the shape is equal.
            }
        }
    }
    if over.is_empty() {
        return Ok(());
    }
    // A timed-out on_main closure may run late. Allow only one outstanding closure, preventing
    // an unserviced main queue from accumulating a new task every 50 ms.
    if pending.swap(true, Ordering::AcqRel) {
        return Err("cursor main-thread read still pending");
    }
    let mut scales: Vec<_> = over.iter().map(|(_, scale, _)| *scale).collect();
    scales.sort_unstable();
    scales.dedup();
    let main_shared = shared.clone();
    let main_cache = cache.clone();
    let main_pending = pending.clone();
    let main_deadline = Instant::now() + MAIN_TIMEOUT;
    let images = on_main(MAIN_TIMEOUT, move |mtm| {
        objc2::rc::autoreleasepool(|_| {
            let result = if Instant::now() >= main_deadline
                || main_shared.shutdown.load(Ordering::Acquire)
                || !main_shared.permitted()
                || !main_shared
                    .deliveries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .any(|d| d.active)
            {
                Err("cursor read cancelled")
            } else {
                read(
                    mtm,
                    &scales,
                    &mut main_cache.lock().unwrap_or_else(|e| e.into_inner()),
                )
            };
            main_pending.store(false, Ordering::Release);
            result
        })
    })
    .map_err(|error| match error {
        PlatformError::Timeout => "cursor main-thread timeout",
        _ => "cursor main-thread dispatch failed",
    })??;
    let mut deliveries = shared.deliveries.lock().unwrap_or_else(|e| e.into_inner());
    if shared.shutdown.load(Ordering::Acquire) || !shared.permitted() {
        return Ok(());
    }
    for (stream, scale, crop) in over {
        let Some(delivery) = deliveries
            .get_mut(&stream)
            .filter(|d| d.active && d.scale.to_bits() == scale && d.cursor.crop == crop)
        else {
            continue;
        };
        if let Some(cursor) = images.get(&scale)
            && delivery.cursor.changed(cursor)
        {
            delivery.sink.send(FrameEvent::Cursor {
                stream,
                cursor: cursor.clone(),
            });
        }
    }
    Ok(())
}

pub(super) fn watch(shared: Arc<Shared>) {
    let cache = Arc::new(Mutex::new(Cache::default()));
    let pending = Arc::new(AtomicBool::new(false));
    let mut last_error: Option<Instant> = None;
    loop {
        let mut deliveries = shared.deliveries.lock().unwrap_or_else(|e| e.into_inner());
        while !shared.shutdown.load(Ordering::Acquire) && !deliveries.values().any(|d| d.active) {
            deliveries = shared
                .cursor_wake
                .wait(deliveries)
                .unwrap_or_else(|e| e.into_inner());
        }
        if shared.shutdown.load(Ordering::Acquire) {
            return;
        }
        drop(deliveries);
        let next = Instant::now() + TICK;
        if let Err(error) = objc2::rc::autoreleasepool(|_| tick(&shared, &cache, &pending)) {
            let now = Instant::now();
            if last_error.is_none_or(|at| now.duration_since(at) >= ERROR_INTERVAL) {
                tracing::debug!(error, "capture cursor tick skipped");
                last_error = Some(now);
            }
        }
        let mut deliveries = shared.deliveries.lock().unwrap_or_else(|e| e.into_inner());
        while !shared.shutdown.load(Ordering::Acquire)
            && deliveries.values().any(|d| d.active)
            && Instant::now() < next
        {
            (deliveries, _) = shared
                .cursor_wake
                .wait_timeout(deliveries, next.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_types::id::{DisplayId, WindowId};

    #[test]
    fn unpremultiply_bgra() {
        let mut pixels = [32, 64, 128, 128, 3, 4, 5, 0, 1, 2, 255, 255];
        unpremultiply(&mut pixels);
        assert_eq!(pixels, [64, 128, 255, 128, 0, 0, 0, 0, 1, 2, 255, 255]);
    }

    #[test]
    fn hidden_detection() {
        assert!(hidden(&[]));
        assert!(hidden(&[255, 255, 255, 0, 0, 0, 0, 0]));
        assert!(hidden(&[0, 0, 0, 1]));
        assert!(!hidden(&[0, 0, 0, 255]));
    }

    #[test]
    fn hotspot_scale_and_clamp() {
        assert_eq!(
            geometry(CGSize::new(16.0, 12.0), CGPoint::new(3.0, 4.0), 2.0),
            Some((PixelSize::new(32, 24), (6, 8)))
        );
        assert_eq!(
            geometry(CGSize::new(16.0, 12.0), CGPoint::new(-1.0, 100.0), 2.0),
            Some((PixelSize::new(32, 24), (0, 23)))
        );
        assert!(geometry(CGSize::new(f64::NAN, 12.0), CGPoint::ZERO, 2.0).is_none());
        assert!(geometry(CGSize::new(16.0, 12.0), CGPoint::ZERO, 0.0).is_none());
    }

    #[test]
    fn downscale_300_by_200() {
        assert_eq!(
            geometry(CGSize::new(300.0, 200.0), CGPoint::new(150.0, 100.0), 1.0),
            Some((PixelSize::new(256, 171), (128, 85)))
        );
    }

    #[test]
    fn display_and_window_content_with_scale_two_crops() {
        let crop = PixelRect::new([20, 40].into(), [100, 120].into());
        for (target, bounds) in [
            (
                CaptureTarget::Display(DisplayId(1)),
                CGRect::new(CGPoint::new(-100.0, 20.0), CGSize::new(200.0, 100.0)),
            ),
            (
                CaptureTarget::Window(WindowId(2)),
                CGRect::new(CGPoint::new(40.0, -20.0), CGSize::new(120.0, 80.0)),
            ),
        ] {
            let state = StreamCursor::new(target, Some(crop));
            let point = |x, y| CGPoint::new(bounds.origin.x + x, bounds.origin.y + y);
            assert!(point_in_content(point(10.0, 20.0), bounds, state.crop, 2.0));
            assert!(point_in_content(point(49.9, 59.9), bounds, state.crop, 2.0));
            assert!(!point_in_content(point(9.9, 20.0), bounds, state.crop, 2.0));
            assert!(!point_in_content(
                point(50.0, 60.0),
                bounds,
                state.crop,
                2.0
            ));
            assert!(!point_in_content(point(-1.0, 0.0), bounds, None, 2.0));
            assert!(!point_in_content(
                point(bounds.size.width, 0.0),
                bounds,
                None,
                2.0
            ));
            assert!(point_in_content(point(0.0, 0.0), bounds, None, 2.0));
        }
    }

    #[test]
    fn suppress_equal_bytes_and_hotspot_until_reentry() {
        let mut state = StreamCursor::new(CaptureTarget::Display(DisplayId(1)), None);
        let mut image = Some(CursorImage {
            size: PixelSize::new(2, 1),
            hotspot: (0, 0),
            pixels: Arc::from([0, 0, 0, 255, 255, 255, 255, 255]),
        });
        assert!(state.changed(&image));
        assert!(!state.changed(&image.clone()));
        if let Some(image) = &mut image {
            image.hotspot = (1, 0);
        }
        assert!(state.changed(&image));
        if let Some(image) = &mut image {
            image.pixels = Arc::from([1, 0, 0, 255, 255, 255, 255, 255]);
        }
        assert!(state.changed(&image));
        assert!(state.changed(&None));
        assert!(!state.changed(&None));
        state.last = None;
        assert!(state.changed(&None));
    }
}
