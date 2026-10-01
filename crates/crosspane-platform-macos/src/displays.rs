//! Read-only display snapshots from Quartz, with optional AppKit names and refresh rates.
//!
//! Quartz documents external reconfiguration notifications as arriving while the application
//! processes events, but does not guarantee callback thread affinity. Registration runs through
//! `on_main` to use the agent's AppKit run loop. The callback supports any thread and only wakes
//! the snapshot worker; its actual delivery thread still needs an attended hot-plug check.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crosspane_platform::{Displays, EventSink, PlatformError};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{DisplayGeometry, PixelSize, PointLogical, SizeMm};
use crosspane_types::id::DisplayId;
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSScreen;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayChangeSummaryFlags, CGDisplayCopyDisplayMode,
    CGDisplayMirrorsDisplay, CGDisplayMode, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback, CGDisplayRotation, CGDisplayScreenSize, CGError,
    CGGetActiveDisplayList, kCGNullDirectDisplay,
};
use objc2_foundation::{NSNumber, NSString};

use crate::main_thread::on_main;

const MAIN_WAIT: Duration = Duration::from_millis(250);
const QUERY_WAIT: Duration = Duration::from_secs(1);
const DEBOUNCE: Duration = Duration::from_millis(200);
const OBSERVATION_CHECK: Duration = Duration::from_secs(2);
const MM_PER_POINT: f64 = 25.4 / 110.0;

type SnapshotReply = SyncSender<Result<Vec<(u32, DisplayNumbers)>, PlatformError>>;

/// A Send handle; native AppKit objects never leave the main thread. Display enumeration and
/// reconfiguration notifications do not capture screen contents or require a TCC grant.
#[derive(Debug)]
pub struct MacDisplays {
    queries: SyncSender<SnapshotReply>,
    subscription: Option<Subscription>,
}

impl MacDisplays {
    pub fn new() -> Result<MacDisplays, PlatformError> {
        let (queries, rx) = mpsc::sync_channel::<SnapshotReply>(1);
        // A single native query thread bounds command waits even if WindowServer stalls, without
        // creating another blocked thread on each retry. It exits when its senders are dropped.
        std::thread::Builder::new()
            .name("mac-display-query".into())
            .spawn(move || {
                while let Ok(reply) = rx.recv() {
                    let _ = reply.send(native_snapshot());
                }
            })
            .map_err(|e| PlatformError::Backend(format!("spawn display query thread: {e}")))?;
        Ok(MacDisplays {
            queries,
            subscription: None,
        })
    }
}

impl Displays for MacDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
        snapshot(&self.queries)
    }

    fn subscribe(
        &mut self,
        sink: Arc<dyn EventSink<Vec<DisplayInfo>>>,
    ) -> Result<(), PlatformError> {
        if self.subscription.is_some() {
            return Err(PlatformError::Backend(
                "Displays::subscribe called twice".into(),
            ));
        }
        let (tx, rx) = mpsc::sync_channel(1);
        // Register before enumerating, so a change during the initial snapshot is queued.
        // A late result after an on_main timeout drops its registration on the main thread.
        let subscription = on_main(MAIN_WAIT, move |_| Subscription::register(tx))??;
        let initial = snapshot(&self.queries)?;
        let stopped = subscription.stopped.clone();
        let queries = self.queries.clone();
        std::thread::Builder::new()
            .name("mac-displays".into())
            .spawn(move || {
                if !stopped.load(Ordering::Acquire) {
                    // One delivery thread keeps the initial snapshot ahead of queued changes.
                    sink.send(initial.clone());
                    worker(rx, &stopped, &*sink, initial, &queries);
                }
            })
            .map_err(|e| PlatformError::Backend(format!("spawn displays thread: {e}")))?;
        self.subscription = Some(subscription);
        Ok(())
    }
}

// Callback userdata is an opaque identity, never dereferenced. This registry makes even a
// callback already in flight during removal safe, without assuming Quartz joins callbacks.
// ponytail: one short registry lock; shard only if concurrent subscribers cause contention.
fn callbacks() -> &'static Mutex<HashMap<usize, SyncSender<()>>> {
    static CALLBACKS: OnceLock<Mutex<HashMap<usize, SyncSender<()>>>> = OnceLock::new();
    CALLBACKS.get_or_init(|| Mutex::new(HashMap::new()))
}

extern "C-unwind" fn reconfigured(
    _display: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
    user_info: *mut c_void,
) {
    // Only the completion notification has up-to-date display state.
    if flags.contains(CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
        return;
    }
    let registry = callbacks().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = registry.get(&(user_info as usize)) {
        // A pending wake already covers this change; never block the Quartz callback.
        let _ = tx.try_send(());
    }
}

#[derive(Debug)]
struct Subscription {
    token: Option<Box<u8>>,
    stopped: Arc<AtomicBool>,
}

impl Subscription {
    fn register(tx: SyncSender<()>) -> Result<Self, PlatformError> {
        let mut token = Box::new(0);
        let user_info = (&mut *token as *mut u8).cast::<c_void>();
        callbacks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(user_info as usize, tx);
        // SAFETY: the callback only looks up this opaque pointer's address; the allocation stays
        // alive until removal. Registration is called by on_main, with an AppKit event loop.
        let status =
            unsafe { CGDisplayRegisterReconfigurationCallback(Some(reconfigured), user_info) };
        if status != CGError::Success {
            callbacks()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(user_info as usize));
            return Err(cg_error("register display callback", status));
        }
        Ok(Self {
            token: Some(token),
            stopped: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let Some(mut token) = self.token.take() else {
            return;
        };
        let key = &mut *token as *mut u8 as usize;
        if let Some(tx) = callbacks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key)
        {
            let _ = tx.try_send(());
        }
        // The closure owns the allocation even if on_main times out and runs it later.
        if let Err(error) = on_main(MAIN_WAIT, move |_| {
            let user_info = (&mut *token as *mut u8).cast::<c_void>();
            // SAFETY: exactly the callback/userdata pair registered above; the token is alive.
            let status =
                unsafe { CGDisplayRemoveReconfigurationCallback(Some(reconfigured), user_info) };
            if status != CGError::Success {
                // ponytail: retain one byte on failed removal so Quartz's opaque identity cannot
                // be reused by another subscription; retry removal if this ever occurs in practice.
                let _ = Box::leak(token);
                tracing::warn!(?status, "remove display callback failed; callback is inert");
            }
        }) {
            tracing::warn!(%error, "display callback cleanup awaiting main thread");
        }
    }
}

fn worker(
    rx: mpsc::Receiver<()>,
    stopped: &AtomicBool,
    sink: &dyn EventSink<Vec<DisplayInfo>>,
    mut last: Vec<DisplayInfo>,
    queries: &SyncSender<SnapshotReply>,
) {
    'observe: loop {
        let notified = match rx.recv_timeout(OBSERVATION_CHECK) {
            Ok(()) => true,
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if notified {
            loop {
                if stopped.load(Ordering::Acquire) {
                    return;
                }
                match rx.recv_timeout(DEBOUNCE) {
                    Ok(()) => continue,
                    Err(RecvTimeoutError::Disconnected) => break 'observe,
                    Err(RecvTimeoutError::Timeout) => break,
                }
            }
        }
        if stopped.load(Ordering::Acquire) {
            return;
        }
        // Quartz has no observation-lost notification. An idle read detects WindowServer loss
        // (empty snapshot), recovery, or a missed change without repeating unchanged snapshots.
        let displays = snapshot(queries).unwrap_or_else(|error| {
            tracing::warn!(%error, "display observation failed; reporting no displays");
            Vec::new()
        });
        if stopped.load(Ordering::Acquire) {
            return;
        }
        if notified || displays != last {
            last = displays.clone();
            sink.send(displays);
        }
    }
    if !stopped.load(Ordering::Acquire) {
        sink.send(Vec::new());
    }
}

#[derive(Debug)]
struct ScreenInfo {
    id: u32,
    name: String,
    max_fps: Option<u32>,
}

fn native_snapshot() -> Result<Vec<(u32, DisplayNumbers)>, PlatformError> {
    let mut count = 0;
    // SAFETY: null with capacity zero requests the count; count is valid writable storage.
    let status = unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) };
    if status != CGError::Success {
        return Err(cg_error("count active displays", status));
    }
    let mut ids = vec![kCGNullDirectDisplay; count as usize];
    if count == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: ids has count initialized writable elements; the API writes at most that capacity.
    let status = unsafe { CGGetActiveDisplayList(count, ids.as_mut_ptr(), &mut count) };
    if status != CGError::Success {
        return Err(cg_error("list active displays", status));
    }
    ids.truncate(count as usize);
    ids.into_iter()
        .filter(|&id| CGDisplayMirrorsDisplay(id) == kCGNullDirectDisplay)
        .map(|id| {
            let bounds = CGDisplayBounds(id);
            let physical = CGDisplayScreenSize(id);
            let mode = CGDisplayCopyDisplayMode(id).ok_or(PlatformError::NotFound)?;
            Ok((
                id,
                DisplayNumbers {
                    origin: (bounds.origin.x, bounds.origin.y),
                    bounds: (bounds.size.width, bounds.size.height),
                    pixels: (
                        CGDisplayMode::pixel_width(Some(&mode)),
                        CGDisplayMode::pixel_height(Some(&mode)),
                    ),
                    rotation: CGDisplayRotation(id),
                    physical_mm: (physical.width, physical.height),
                    refresh_hz: CGDisplayMode::refresh_rate(Some(&mode)),
                },
            ))
        })
        .collect()
}

fn snapshot(queries: &SyncSender<SnapshotReply>) -> Result<Vec<DisplayInfo>, PlatformError> {
    let (tx, rx) = mpsc::sync_channel(1);
    queries.try_send(tx).map_err(|error| match error {
        mpsc::TrySendError::Full(_) => PlatformError::Timeout,
        mpsc::TrySendError::Disconnected(_) => {
            PlatformError::Backend("display query worker disconnected".into())
        }
    })?;
    let numbers = rx.recv_timeout(QUERY_WAIT).map_err(|error| match error {
        RecvTimeoutError::Timeout => PlatformError::Timeout,
        RecvTimeoutError::Disconnected => {
            PlatformError::Backend("display query worker disconnected".into())
        }
    })??;
    if numbers.is_empty() {
        return Ok(Vec::new());
    }
    let screens = match on_main(MAIN_WAIT, |mtm| {
        autoreleasepool(|_| {
            let key = NSString::from_str("NSScreenNumber");
            NSScreen::screens(mtm)
                .iter()
                .filter_map(|screen| {
                    let description = screen.deviceDescription();
                    let number = description.objectForKey(&key)?;
                    let id = number.downcast_ref::<NSNumber>()?.unsignedIntValue();
                    Some(ScreenInfo {
                        id,
                        name: screen.localizedName().to_string(),
                        max_fps: u32::try_from(screen.maximumFramesPerSecond()).ok(),
                    })
                })
                .collect::<Vec<_>>()
        })
    }) {
        Ok(screens) => screens,
        Err(PlatformError::Timeout) => Vec::new(), // libtest has no AppKit run loop.
        Err(error) => return Err(error),
    };
    numbers
        .into_iter()
        .map(|(id, numbers)| {
            map_display(id, numbers, screens.iter().find(|screen| screen.id == id))
        })
        .collect()
}

fn cg_error(operation: &str, status: CGError) -> PlatformError {
    PlatformError::Backend(format!("{operation}: CGError {}", status.0))
}

#[derive(Clone, Copy, Debug)]
struct DisplayNumbers {
    origin: (f64, f64),
    bounds: (f64, f64),
    pixels: (usize, usize),
    rotation: f64,
    physical_mm: (f64, f64),
    refresh_hz: f64,
}

fn map_display(
    id: u32,
    numbers: DisplayNumbers,
    screen: Option<&ScreenInfo>,
) -> Result<DisplayInfo, PlatformError> {
    let invalid = || PlatformError::Backend(format!("invalid display metrics for {id}"));
    if !numbers.bounds.0.is_finite()
        || !numbers.bounds.1.is_finite()
        || numbers.bounds.0 <= 0.0
        || numbers.bounds.1 <= 0.0
        || !numbers.rotation.is_finite()
        || !numbers.refresh_hz.is_finite()
        || numbers.refresh_hz < 0.0
    {
        return Err(invalid());
    }
    let mut pixels = PixelSize::new(
        u32::try_from(numbers.pixels.0).map_err(|_| invalid())?,
        u32::try_from(numbers.pixels.1).map_err(|_| invalid())?,
    );
    let mut physical = SizeMm::new(numbers.physical_mm.0, numbers.physical_mm.1);
    let rotation = numbers.rotation.rem_euclid(360.0);
    if rotation == 90.0 || rotation == 270.0 {
        std::mem::swap(&mut pixels.width, &mut pixels.height);
        std::mem::swap(&mut physical.width, &mut physical.height);
    }
    if physical.width == 0.0 || physical.height == 0.0 {
        // Bounds are already rotated; do not swap this fallback a second time.
        physical = SizeMm::new(
            numbers.bounds.0 * MM_PER_POINT,
            numbers.bounds.1 * MM_PER_POINT,
        );
    }
    let geometry = DisplayGeometry {
        physical_size: physical,
        pixel_size: pixels,
        scale: f64::from(pixels.width) / numbers.bounds.0,
        logical_origin: PointLogical::new(numbers.origin.0, numbers.origin.1),
    };
    let refresh = (numbers.refresh_hz * 1000.0).round();
    if !geometry.is_valid() || !refresh.is_finite() || refresh > f64::from(u32::MAX) {
        return Err(invalid());
    }
    let refresh_millihz = if refresh == 0.0 {
        screen
            .and_then(|screen| screen.max_fps)
            .and_then(|fps| fps.checked_mul(1000))
            .filter(|&rate| rate > 0)
            .unwrap_or(60_000)
    } else {
        refresh as u32
    };
    Ok(DisplayInfo {
        id: DisplayId(id),
        name: screen.map_or_else(|| format!("Display {id}"), |screen| screen.name.clone()),
        geometry,
        refresh_millihz,
        color_space: ColorSpace::Srgb,
        hdr: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_mode_rotation_and_fallbacks() {
        let base = DisplayNumbers {
            origin: (-1512.0, 100.0),
            bounds: (1512.0, 982.0),
            pixels: (3024, 1964),
            rotation: 0.0,
            physical_mm: (302.0, 196.0),
            refresh_hz: 59.9406,
        };
        let screen = ScreenInfo {
            id: 1,
            name: "Built-in".into(),
            max_fps: Some(120),
        };
        let display = map_display(1, base, Some(&screen)).unwrap();
        assert_eq!(display.id, DisplayId(1));
        assert_eq!(display.name, "Built-in");
        assert_eq!(display.geometry.scale, 2.0);
        assert_eq!(display.geometry.pixel_size, PixelSize::new(3024, 1964));
        assert_eq!(display.geometry.physical_size, SizeMm::new(302.0, 196.0));
        assert_eq!(
            display.geometry.logical_origin,
            PointLogical::new(-1512.0, 100.0)
        );
        assert_eq!(display.refresh_millihz, 59_941);
        assert_eq!(display.color_space, ColorSpace::Srgb);
        assert!(!display.hdr);
        assert!(display.geometry.is_valid());

        for rotation in [90.0, 270.0] {
            let rotated = DisplayNumbers {
                bounds: (982.0, 1512.0),
                rotation,
                ..base
            };
            let display = map_display(1, rotated, None).unwrap();
            assert_eq!(display.geometry.pixel_size, PixelSize::new(1964, 3024));
            assert_eq!(display.geometry.physical_size, SizeMm::new(196.0, 302.0));
            assert_eq!(display.geometry.scale, 2.0);
            let fallback = map_display(
                1,
                DisplayNumbers {
                    physical_mm: (0.0, 0.0),
                    refresh_hz: 0.0,
                    ..rotated
                },
                None,
            )
            .unwrap();
            assert_eq!(fallback.name, "Display 1");
            assert!((fallback.geometry.physical_size.width - 982.0 * 25.4 / 110.0).abs() < 1e-9);
            assert!((fallback.geometry.physical_size.height - 1512.0 * 25.4 / 110.0).abs() < 1e-9);
            assert_eq!(fallback.refresh_millihz, 60_000);
            assert!(fallback.geometry.is_valid());
        }
        assert_eq!(
            map_display(
                1,
                DisplayNumbers {
                    refresh_hz: 0.0,
                    ..base
                },
                Some(&screen)
            )
            .unwrap()
            .refresh_millihz,
            120_000
        );
        let fractional = map_display(
            1,
            DisplayNumbers {
                bounds: (1920.0, 1247.0),
                ..base
            },
            None,
        )
        .unwrap();
        assert_eq!(fractional.geometry.scale, 3024.0 / 1920.0);
        assert!(
            map_display(
                1,
                DisplayNumbers {
                    bounds: (0.0, 1.0),
                    ..base
                },
                None
            )
            .is_err()
        );
        assert!(
            map_display(
                1,
                DisplayNumbers {
                    pixels: (0, 1964),
                    ..base
                },
                None
            )
            .is_err()
        );
        assert!(
            map_display(
                1,
                DisplayNumbers {
                    refresh_hz: f64::NAN,
                    ..base
                },
                None
            )
            .is_err()
        );
    }
}
