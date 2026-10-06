//! Opt-in CGVirtualDisplay parking (D7). No private symbols are linked.
//! The retained displays live only on the main thread and die with their process;
//! after a crash, recovery needs only the original window frames in the journal.
//! macOS keeps arrangements contiguous. Try the four corners of the real arrangement,
//! preferring top-right, so the twin touches other displays only at a point. In v0, if
//! WindowServer normalizes every corner into edge adjacency, accept its reported origin
//! and warn: the physical pointer can reach the twin. No input is used to test isolation.
//! Descriptor capacity is fixed at 8192×8192 pixels so modes can grow after parking.
//! The macOS 27 lifecycle probe confirmed growth to that full capacity at 2×.
//! Destination scales above 1 use a 2× twin (even for fractional scales or scales above 2).
//! applySettings publishes modes but may leave the old mode selected; public CoreGraphics
//! selects the requested mode before its pixel and logical dimensions are polled.
//! Parking operations are serialized across native waits and must run on a worker thread,
//! never the AppKit main thread, which must keep servicing on_main/spawn_on_main.
//! A window that goes fullscreen on its twin keeps projecting (docs/wp/FULLSCREEN-design.md §6).
//! The fullscreen window is the twin itself, or a same-process stand-in that fills it (WebKit's
//! title-less fullscreen window): AX can't move it, and the page's own window is off-Space, so
//! the AX lookup misses. A miss never releases the display while the Quartz window exists; only
//! its absence, or a revoked Accessibility permission, does.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crosspane_platform::{Parked, ParkingKind, PlatformError, WindowParking};
use crosspane_types::geom::{PixelRect, PixelSize, PointLogical, RectLogical, SizeLogical, euclid};
use crosspane_types::id::{DisplayId, WindowId};
use dispatch2::DispatchQueue;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, Bool, Method, Sel};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_app_kit::NSScreen;
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CGSize};
use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration,
    CGConfigureDisplayOrigin, CGConfigureOption, CGDisplayBounds, CGDisplayCopyAllDisplayModes,
    CGDisplayCopyDisplayMode, CGDisplayMirrorsDisplay, CGDisplayMode, CGDisplayModelNumber,
    CGDisplaySetDisplayMode, CGDisplayVendorNumber, CGError, CGGetActiveDisplayList,
    kCGDisplayShowDuplicateLowResolutionModes, kCGNullDirectDisplay,
};
use objc2_foundation::{NSArray, NSNumber, NSString};

use crate::main_thread::{on_main, spawn_on_main};
use crate::windows::{
    AxLookup, AxWindow, FULLSCREEN_WAIT, RawWindow, WindowQuery, bounds_equal,
    ensure_fullscreen_with, fullscreen_pause, fullscreen_press_needed, require_accessibility,
    valid_frame,
};

const MAIN_WAIT: Duration = Duration::from_secs(2);
const AX_WAIT: Duration = Duration::from_secs(2);
const DISPLAY_WAIT: Duration = Duration::from_secs(1);
const INSET_WAIT: Duration = Duration::from_millis(500);
const INSET_POLL: Duration = Duration::from_millis(50);
const MAX_PIXELS: u32 = 8192;
const MAX_FAILED_STARTS: u8 = 3;

thread_local! {
    // Accessed exclusively inside on_main/spawn_on_main. Native objects never cross threads.
    static DISPLAYS: RefCell<BTreeMap<u32, (DisplayId, Retained<AnyObject>)>> = const { RefCell::new(BTreeMap::new()) };
}

fn classes() -> Result<[&'static AnyClass; 4], PlatformError> {
    let missing = || PlatformError::Unsupported("CGVirtualDisplay unavailable");
    let classes = [
        AnyClass::get(c"CGVirtualDisplay").ok_or_else(missing)?,
        AnyClass::get(c"CGVirtualDisplayDescriptor").ok_or_else(missing)?,
        AnyClass::get(c"CGVirtualDisplaySettings").ok_or_else(missing)?,
        AnyClass::get(c"CGVirtualDisplayMode").ok_or_else(missing)?,
    ];
    let drift = || PlatformError::Unsupported("CGVirtualDisplay API changed");
    for class in classes {
        // Check the public class-message ABI before sending the capability query or allocating.
        for (selector, expected) in [
            (sel!(alloc), c"@16@0:8"),
            (sel!(instancesRespondToSelector:), c"B24@0:8:16"),
        ] {
            if !encoding_matches(
                method_encoding(class.class_method(selector).ok_or_else(drift)?),
                expected,
            ) {
                return Err(drift());
            }
        }
    }
    for (class, selector, expected) in api_methods(classes) {
        let method = class.instance_method(selector).ok_or_else(drift)?;
        if !encoding_matches(method_encoding(method), expected) {
            return Err(drift());
        }
        // SAFETY: NSObject +instancesRespondToSelector:(SEL) -> BOOL, checked above as B24@0:8:16.
        let responds: Bool = unsafe { msg_send![class, instancesRespondToSelector: selector] };
        if !responds.as_bool() {
            return Err(drift());
        }
    }
    Ok(classes)
}

fn api_methods(
    [display, descriptor, settings, mode]: [&'static AnyClass; 4],
) -> [(&'static AnyClass, Sel, &'static CStr); 16] {
    [
        (descriptor, sel!(init), c"@16@0:8"),
        (descriptor, sel!(setQueue:), c"v24@0:8@16"),
        (descriptor, sel!(setName:), c"v24@0:8@16"),
        (descriptor, sel!(setMaxPixelsWide:), c"v20@0:8I16"),
        (descriptor, sel!(setMaxPixelsHigh:), c"v20@0:8I16"),
        (
            descriptor,
            sel!(setSizeInMillimeters:),
            c"v32@0:8{CGSize=dd}16",
        ),
        (descriptor, sel!(setProductID:), c"v20@0:8I16"),
        (descriptor, sel!(setVendorID:), c"v20@0:8I16"),
        (descriptor, sel!(setSerialNum:), c"v20@0:8I16"),
        (settings, sel!(init), c"@16@0:8"),
        (settings, sel!(setHiDPI:), c"v20@0:8I16"),
        (settings, sel!(setModes:), c"v24@0:8@16"),
        (
            mode,
            sel!(initWithWidth:height:refreshRate:),
            c"@32@0:8I16I20d24",
        ),
        (display, sel!(initWithDescriptor:), c"@24@0:8@16"),
        (display, sel!(applySettings:), c"B24@0:8@16"),
        (display, sel!(displayID), c"I16@0:8"),
    ]
}

fn method_encoding(method: &Method) -> Option<&CStr> {
    // SAFETY: method is a live runtime Method from class_getInstanceMethod/class_getClassMethod;
    // method_getTypeEncoding returns a runtime-owned immutable string or null.
    let raw = unsafe { objc2::ffi::method_getTypeEncoding(method) };
    if raw.is_null() {
        None
    } else {
        // SAFETY: the non-null runtime encoding is NUL-terminated and lives as long as the Method.
        Some(unsafe { CStr::from_ptr(raw) })
    }
}

fn encoding_matches(observed: Option<&CStr>, expected: &CStr) -> bool {
    observed == Some(expected)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Mode {
    pixels: PixelSize,
    width: u32,
    height: u32,
    hidpi: u32,
}

impl Mode {
    fn new(pixels: PixelSize, scale: f64) -> Result<Self, PlatformError> {
        if pixels.width == 0 || pixels.height == 0 || !scale.is_finite() || scale <= 0.0 {
            return Err(PlatformError::Backend("invalid twin size or scale".into()));
        }
        if pixels.width > MAX_PIXELS || pixels.height > MAX_PIXELS {
            return Err(PlatformError::Unsupported(
                "CGVirtualDisplay maximum is 8192x8192 pixels",
            ));
        }
        // The API offers 1× or 2× density. Round odd HiDPI sizes up to retain every pixel.
        let hidpi = u32::from(scale > 1.0);
        let density = if hidpi == 1 { 2 } else { 1 };
        let width = pixels.width.div_ceil(density);
        let height = pixels.height.div_ceil(density);
        let pixels = PixelSize::new(width * density, height * density);
        Ok(Self {
            pixels,
            width,
            height,
            hidpi,
        })
    }

    fn logical(self) -> SizeLogical {
        SizeLogical::new(f64::from(self.width), f64::from(self.height))
    }

    /// Reserve logical points above the requested window, retaining the API's density/rounding.
    fn with_top_inset(self, inset: f64) -> Result<Self, PlatformError> {
        let density = if self.hidpi == 1 { 2.0 } else { 1.0 };
        let height = ((f64::from(self.height) + inset) * density).ceil();
        if height > f64::from(MAX_PIXELS) {
            return Err(PlatformError::Unsupported(
                "CGVirtualDisplay maximum is 8192x8192 pixels",
            ));
        }
        Self::new(PixelSize::new(self.pixels.width, height as u32), density)
    }

    /// Once a valid mode exists, a reservation that won't fit must not tear down parking.
    fn fitting_top_inset(self, inset: f64, fallback: (Self, f64)) -> (Self, f64) {
        self.with_top_inset(inset)
            .map(|mode| (mode, inset))
            .unwrap_or(fallback)
    }
}

/// AppKit's Y axis points up: only the space above visibleFrame is the top reservation.
fn top_inset(frame_max_y: f64, visible_max_y: f64) -> Option<f64> {
    let inset = frame_max_y - visible_max_y;
    inset.is_finite().then(|| inset.clamp(0.0, 64.0))
}

#[derive(Debug, PartialEq)]
enum InsetRead {
    Measured(f64),
    Retry(Duration),
    Fallback,
}

fn inset_read(
    observed: Option<(SizeLogical, f64)>,
    expected: SizeLogical,
    remaining: Duration,
) -> InsetRead {
    if remaining.is_zero() {
        return InsetRead::Fallback;
    }
    if let Some((size, inset)) = observed
        && size == expected
    {
        return InsetRead::Measured(inset);
    }
    InsetRead::Retry(remaining.min(INSET_POLL))
}

fn measure_top_inset(id: DisplayId, expected: SizeLogical) -> (Option<f64>, u32) {
    let deadline = Instant::now() + INSET_WAIT;
    let mut tries = 0;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return (None, tries);
        }
        tries += 1;
        let observed = on_main(remaining, move |mtm| {
            autoreleasepool(|_| {
                let key = NSString::from_str("NSScreenNumber");
                NSScreen::screens(mtm).iter().find_map(|screen| {
                    let description = screen.deviceDescription();
                    let number = description.objectForKey(&key)?;
                    if number.downcast_ref::<NSNumber>()?.unsignedIntValue() != id.0 {
                        return None;
                    }
                    let frame = screen.frame();
                    let visible = screen.visibleFrame();
                    let inset = top_inset(
                        frame.origin.y + frame.size.height,
                        visible.origin.y + visible.size.height,
                    )?;
                    Some((SizeLogical::new(frame.size.width, frame.size.height), inset))
                })
            })
        })
        .ok()
        .flatten();
        match inset_read(
            observed,
            expected,
            deadline.saturating_duration_since(Instant::now()),
        ) {
            InsetRead::Measured(inset) => return (Some(inset), tries),
            // Yield between separate main-queue calls so AppKit can process screen notifications.
            InsetRead::Retry(wait) => std::thread::sleep(wait),
            InsetRead::Fallback => return (None, tries),
        }
    }
}

fn remember_top_inset(last: &mut f64, measured: Option<f64>) -> f64 {
    if let Some(inset) = measured {
        *last = inset;
    }
    *last
}

/// CoreGraphics' Y axis points down. The window keeps its requested size below the reservation.
fn target_rect(bounds: RectLogical, inset: f64, size: SizeLogical) -> RectLogical {
    RectLogical::new(
        PointLogical::new(bounds.min_x(), bounds.min_y() + inset),
        size,
    )
}

fn settings(mode: Mode) -> Result<Retained<AnyObject>, PlatformError> {
    let [_, _, settings_class, mode_class] = classes()?;
    // SAFETY: -init returns an owned settings object. On macOS 27, the runtime encodes
    // -initWithWidth:height:refreshRate: as (unsigned int, unsigned int, double), returning
    // an owned mode. DeskPad's NSUInteger declaration is wider than the observed ABI.
    let (settings, native_mode): (Option<Retained<AnyObject>>, Option<Retained<AnyObject>>) = unsafe {
        (
            msg_send![msg_send![settings_class, alloc], init],
            msg_send![msg_send![mode_class, alloc], initWithWidth: mode.width, height: mode.height, refreshRate: 60.0_f64],
        )
    };
    let settings =
        settings.ok_or_else(|| PlatformError::Backend("create virtual display settings".into()))?;
    let native_mode =
        native_mode.ok_or_else(|| PlatformError::Backend("create virtual display mode".into()))?;
    let modes = NSArray::from_slice(&[&*native_mode]);
    // SAFETY: -setHiDPI:(unsigned int) and -setModes:(NSArray<CGVirtualDisplayMode *> *)
    // are the property setters declared in DeskPad's CGVirtualDisplayPrivate.h.
    unsafe {
        let _: () = msg_send![&*settings, setHiDPI: mode.hidpi];
        let _: () = msg_send![&*settings, setModes: &*modes];
    }
    Ok(settings)
}

fn apply(display: &AnyObject, mode: Mode) -> Result<(), PlatformError> {
    let settings = settings(mode)?;
    // SAFETY: -applySettings:(CGVirtualDisplaySettings *) returns BOOL per DeskPad's header.
    let success: Bool = unsafe { msg_send![display, applySettings: &*settings] };
    if !success.as_bool() {
        return Err(PlatformError::Backend(
            "CGVirtualDisplay rejected settings".into(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct VirtualDisplay {
    serial: u32,
    id: DisplayId,
    inset: f64,
}

impl VirtualDisplay {
    fn create(mode: Mode) -> Result<Self, PlatformError> {
        let deadline = Instant::now() + MAIN_WAIT;
        on_main(MAIN_WAIT, move |_| {
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            let [display_class, descriptor_class, _, _] = classes()?;
            static SERIAL: AtomicU32 = AtomicU32::new(1);
            let serial = SERIAL
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
                .map_err(|_| PlatformError::Backend("virtual display serials exhausted".into()))?;
            // SAFETY: NSObject +alloc/-init on CGVirtualDisplayDescriptor returns an owned descriptor.
            let descriptor: Option<Retained<AnyObject>> =
                unsafe { msg_send![msg_send![descriptor_class, alloc], init] };
            let descriptor = descriptor.ok_or_else(|| {
                PlatformError::Backend("create virtual display descriptor".into())
            })?;
            let name = NSString::from_str(&format!("Crosspane twin {serial}"));
            let logical = mode.logical();
            // SAFETY: DeskPad's header declares setQueue:(dispatch_queue_t), setName:(NSString *),
            // setMaxPixelsWide:/High:, setProductID:/VendorID:/SerialNum: (unsigned int), and
            // setSizeInMillimeters:(CGSize). All object arguments remain alive through the calls.
            unsafe {
                let _: () = msg_send![&*descriptor, setQueue: DispatchQueue::main() as *const DispatchQueue];
                let _: () = msg_send![&*descriptor, setName: &*name];
                let _: () = msg_send![&*descriptor, setMaxPixelsWide: MAX_PIXELS];
                let _: () = msg_send![&*descriptor, setMaxPixelsHigh: MAX_PIXELS];
                let _: () = msg_send![&*descriptor, setSizeInMillimeters: CGSize::new(logical.width * 25.4 / 110.0, logical.height * 25.4 / 110.0)];
                let _: () = msg_send![&*descriptor, setProductID: crate::displays::TWIN_PRODUCT];
                let _: () = msg_send![&*descriptor, setVendorID: crate::displays::TWIN_VENDOR];
                let _: () = msg_send![&*descriptor, setSerialNum: serial];
            }
            // SAFETY: -initWithDescriptor:(CGVirtualDisplayDescriptor *) returns an owned display.
            let display: Option<Retained<AnyObject>> = unsafe {
                msg_send![msg_send![display_class, alloc], initWithDescriptor: &*descriptor]
            };
            let display =
                display.ok_or_else(|| PlatformError::Backend("create CGVirtualDisplay".into()))?;
            apply(&display, mode)?;
            // SAFETY: -displayID returns CGDirectDisplayID (uint32_t), per DeskPad's header.
            let id: u32 = unsafe { msg_send![&*display, displayID] };
            if id == 0 {
                return Err(PlatformError::Backend("virtual display has no ID".into()));
            }
            DISPLAYS.with(|displays| {
                displays
                    .borrow_mut()
                    .insert(serial, (DisplayId(id), display))
            });
            // If on_main's receiver timed out, this Send handle is dropped and queues removal.
            Ok(Self {
                serial,
                id: DisplayId(id),
                inset: 0.0,
            })
        })?
    }

    fn apply(&self, mode: Mode) -> Result<(), PlatformError> {
        let serial = self.serial;
        let deadline = Instant::now() + MAIN_WAIT;
        on_main(MAIN_WAIT, move |_| {
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            DISPLAYS.with(|displays| {
                let displays = displays.borrow();
                apply(
                    &displays.get(&serial).ok_or(PlatformError::NotFound)?.1,
                    mode,
                )
            })
        })?
    }

    fn release(self) -> Result<(), PlatformError> {
        let serial = self.serial;
        on_main(MAIN_WAIT, move |_| {
            DISPLAYS.with(|displays| displays.borrow_mut().remove(&serial));
        })?;
        wait_display(self.id, false)
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        let serial = self.serial;
        spawn_on_main(move |_| {
            DISPLAYS.with(|displays| displays.borrow_mut().remove(&serial));
        });
    }
}

fn cg_result(status: CGError, operation: &str) -> Result<(), PlatformError> {
    if status == CGError::Success {
        Ok(())
    } else {
        Err(PlatformError::Backend(format!(
            "{operation}: CGError {}",
            status.0
        )))
    }
}

fn active_displays() -> Result<Vec<u32>, PlatformError> {
    let mut count = 0;
    cg_result(
        // SAFETY: zero capacity with null array queries the count into valid writable storage.
        unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) },
        "active display count",
    )?;
    let mut displays = vec![0; count as usize];
    cg_result(
        // SAFETY: the array has count writable slots; CoreGraphics never exceeds that capacity.
        unsafe { CGGetActiveDisplayList(count, displays.as_mut_ptr(), &mut count) },
        "active displays",
    )?;
    displays.truncate(count as usize);
    Ok(displays)
}

fn wait_display(id: DisplayId, present: bool) -> Result<(), PlatformError> {
    let deadline = Instant::now() + DISPLAY_WAIT;
    loop {
        if active_displays()?.contains(&id.0) == present {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_mode(id: DisplayId, mode: Mode) -> Result<(), PlatformError> {
    let deadline = Instant::now() + DISPLAY_WAIT;
    let mut selected = false;
    let options = CFDictionary::from_slices(
        // SAFETY: the public immutable CoreGraphics option key is available since macOS 10.8.
        &[unsafe { kCGDisplayShowDuplicateLowResolutionModes }],
        &[CFBoolean::new(true)],
    );
    loop {
        if active_displays()?.contains(&id.0)
            && let Some(actual) = CGDisplayCopyDisplayMode(id.0)
        {
            let bounds = CGDisplayBounds(id.0);
            let logical = mode.logical();
            if matches_mode(&actual, mode)
                && bounds.size.width == logical.width
                && bounds.size.height == logical.height
            {
                return Ok(());
            }
            if !selected {
                // SAFETY: the options contain the public CFString key with a CFBoolean value.
                let modes =
                    unsafe { CGDisplayCopyAllDisplayModes(id.0, Some(options.as_opaque())) };
                if let Some(modes) = modes {
                    // SAFETY: CGDisplayCopyAllDisplayModes returns an array of CGDisplayModeRef.
                    let modes: CFRetained<CFArray<CGDisplayMode>> =
                        unsafe { CFRetained::cast_unchecked(modes) };
                    for candidate in &*modes {
                        if matches_mode(&candidate, mode) {
                            cg_result(
                                // SAFETY: id belongs to our twin, the mode is from its advertised list,
                                // and no options dictionary is supplied. This is a process-lived change.
                                unsafe { CGDisplaySetDisplayMode(id.0, Some(&candidate), None) },
                                "select twin mode",
                            )?;
                            selected = true;
                            break;
                        }
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn matches_mode(actual: &CGDisplayMode, mode: Mode) -> bool {
    CGDisplayMode::pixel_width(Some(actual)) == mode.pixels.width as usize
        && CGDisplayMode::pixel_height(Some(actual)) == mode.pixels.height as usize
        && CGDisplayMode::width(Some(actual)) == mode.width as usize
        && CGDisplayMode::height(Some(actual)) == mode.height as usize
}

fn display_frame(id: u32) -> Result<RectLogical, PlatformError> {
    let bounds = CGDisplayBounds(id);
    let frame = RectLogical::new(
        PointLogical::new(bounds.origin.x, bounds.origin.y),
        SizeLogical::new(bounds.size.width, bounds.size.height),
    );
    if !valid_frame(frame) {
        return Err(PlatformError::NotFound);
    }
    Ok(frame)
}

fn corner_only(twin: RectLogical, others: &[RectLogical]) -> bool {
    let mut touches = false;
    for other in others {
        let overlap_x = twin.max_x().min(other.max_x()) - twin.min_x().max(other.min_x());
        let overlap_y = twin.max_y().min(other.max_y()) - twin.min_y().max(other.min_y());
        if overlap_x >= 0.0 && overlap_y >= 0.0 {
            if overlap_x != 0.0 || overlap_y != 0.0 {
                return false;
            }
            touches = true;
        }
    }
    touches
}

fn configure_origin(id: DisplayId, origin: PointLogical) -> Result<(), PlatformError> {
    if [origin.x, origin.y]
        .iter()
        .any(|v| !v.is_finite() || *v < f64::from(i32::MIN) || *v > f64::from(i32::MAX))
    {
        return Err(PlatformError::Backend(
            "twin origin exceeds native coordinates".into(),
        ));
    }
    let mut config = std::ptr::null_mut();
    cg_result(
        // SAFETY: writable CGDisplayConfigRef output; success creates a configuration transaction.
        unsafe { CGBeginDisplayConfiguration(&mut config) },
        "begin display configuration",
    )?;
    // SAFETY: config is a live transaction, id is active, and x/y are representable coordinates.
    let status =
        unsafe { CGConfigureDisplayOrigin(config, id.0, origin.x as i32, origin.y as i32) };
    if let Err(error) = cg_result(status, "configure twin origin") {
        // SAFETY: cancels the live, uncommitted configuration returned by CGBeginDisplayConfiguration.
        let _ = unsafe { CGCancelDisplayConfiguration(config) };
        return Err(error);
    }
    cg_result(
        // SAFETY: consumes the live transaction; session-only configuration is never made persistent.
        unsafe { CGCompleteDisplayConfiguration(config, CGConfigureOption::ForSession) },
        "complete display configuration",
    )
}

#[derive(Clone, Copy, Debug)]
struct Placement {
    frame: RectLogical,
    corner_only: bool,
    attempt: &'static str,
}

fn place_twin(id: DisplayId) -> Result<Placement, PlatformError> {
    let owned = on_main(MAIN_WAIT, |_| {
        DISPLAYS.with(|displays| {
            displays
                .borrow()
                .values()
                .map(|(id, _)| id.0)
                .collect::<Vec<_>>()
        })
    })?;
    let real = active_displays()?
        .into_iter()
        .filter(|display| *display != id.0 && !owned.contains(display))
        .map(display_frame)
        .collect::<Result<Vec<_>, _>>()?;
    let bounds = real
        .into_iter()
        .reduce(|a, b| a.union(&b))
        .ok_or(PlatformError::NotFound)?;
    let size = display_frame(id.0)?.size;
    let candidates = [
        (
            "top-right",
            PointLogical::new(bounds.max_x(), bounds.min_y() - size.height),
        ),
        (
            "top-left",
            PointLogical::new(bounds.min_x() - size.width, bounds.min_y() - size.height),
        ),
        (
            "bottom-left",
            PointLogical::new(bounds.min_x() - size.width, bounds.max_y()),
        ),
        (
            "bottom-right",
            PointLogical::new(bounds.max_x(), bounds.max_y()),
        ),
    ];
    let mut placement = Placement {
        frame: display_frame(id.0)?,
        corner_only: false,
        attempt: "top-right",
    };
    for (attempt, origin) in candidates {
        configure_origin(id, origin)?;
        // Configuration/readback can be asynchronous. Allow up to one second for a corner.
        let deadline = Instant::now() + DISPLAY_WAIT;
        loop {
            let frame = display_frame(id.0)?;
            let others = active_displays()?
                .into_iter()
                .filter(|display| *display != id.0)
                .map(display_frame)
                .collect::<Result<Vec<_>, _>>()?;
            placement = Placement {
                frame,
                corner_only: corner_only(frame, &others),
                attempt,
            };
            if placement.corner_only {
                return Ok(placement);
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    tracing::warn!(
        display = id.0,
        attempt = placement.attempt,
        x = placement.frame.origin.x,
        y = placement.frame.origin.y,
        "twin is edge-adjacent after all four corner attempts; the physical pointer can reach it (v0 limitation)"
    );
    Ok(placement)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Entry {
    pid: i32,
    frame: RectLogical,
    failed_starts: u8,
}

struct ParkAttempt {
    write_attempted: bool,
    forget_entry: bool,
}

impl ParkAttempt {
    fn new(inserted: bool, current: RectLogical, journaled: RectLogical) -> Self {
        Self {
            write_attempted: false,
            // A prior failed park may have moved this window: only proven-home entries are safe.
            forget_entry: inserted || same_frame(current, journaled),
        }
    }
}

/// The window that fills one of `displays` (bounds, in the global logical space), if any: the
/// tracked `window` itself, or else an on-screen window of the same process (a fullscreen stand-in
/// such as WebKit's). A window of another process never counts, and neither does one that isn't on
/// screen. `list` is the Quartz list, `window` the tracked window's entry in it. Independent of
/// whether a twin is owned: after a release, after a restart, and for a window that isn't parked
/// yet, the displays are the active ones (`CGGetActiveDisplayList` + `CGDisplayBounds`); while
/// parked, the twin alone.
fn fullscreen_on_any_display(
    window: &RawWindow,
    list: &[RawWindow],
    displays: &[RectLogical],
) -> Option<WindowId> {
    let fills = |w: &RawWindow| {
        w.on_screen
            && displays
                .iter()
                .any(|display| bounds_equal(w.frame, *display))
    };
    if fills(window) {
        return Some(window.id);
    }
    list.iter()
        .find(|w| w.pid == window.pid && w.id != window.id && fills(w))
        .map(|w| w.id)
}

/// The bounds of every active display.
fn active_display_bounds() -> Result<Vec<RectLogical>, PlatformError> {
    Ok(active_displays()?
        .into_iter()
        .filter_map(|id| display_frame(id).ok())
        .collect())
}

/// Fresh numeric bounds, using the same Crosspane twin identity as the display source.
fn real_display_bounds() -> Result<Vec<RectLogical>, PlatformError> {
    active_displays()?
        .into_iter()
        .filter(|&id| CGDisplayMirrorsDisplay(id) == kCGNullDirectDisplay)
        .filter(|&id| {
            CGDisplayVendorNumber(id) != crate::displays::TWIN_VENDOR
                || CGDisplayModelNumber(id) != crate::displays::TWIN_PRODUCT
        })
        .map(display_frame)
        .collect()
}

#[derive(Clone, Copy)]
enum RecoveryLocation {
    Real,
    Elsewhere,
    Gone,
}

fn on_real_display(raw: &RawWindow, displays: &[RectLogical]) -> bool {
    raw.on_screen
        && valid_frame(raw.frame)
        && displays
            .iter()
            .any(|display| valid_frame(*display) && display.contains_rect(&raw.frame))
}

/// Where a window stands with respect to native fullscreen, on any display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    /// Nothing fills a display on its behalf.
    Clear,
    /// The window itself fills a display: its full-screen button can take it out.
    Itself,
    /// A same-process stand-in fills a display (WebKit's title-less fullscreen window): there is
    /// no button, the window's own AX element isn't reachable, and writing the stand-in moves the
    /// wrong window.
    StandIn,
}

fn fullscreen_gate(window: &RawWindow, list: &[RawWindow], displays: &[RectLogical]) -> Gate {
    match fullscreen_on_any_display(window, list, displays) {
        None => Gate::Clear,
        Some(id) if id == window.id => Gate::Itself,
        Some(_) => Gate::StandIn,
    }
}

/// What an observation of the parked window (Quartz, or AX after a miss) means for the twin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AxMiss {
    /// The window still exists, or we couldn't tell: it may be on another Space or covered by a
    /// fullscreen stand-in. Report its last known geometry and keep the display.
    Keep,
    /// The window is confirmed gone, or Accessibility was revoked: release the display (04 §8
    /// invariant 4).
    Release,
}

/// `lookup` is the result of asking Quartz for the parked window. Only a confirmed `NotFound` (the
/// window is absent from a list that includes every Space) or a revoked permission releases the
/// display. A failed or timed-out read (a WindowServer stall during a Space transition) says
/// nothing about the window, so it keeps the display like a window that exists.
fn ax_miss_decision(lookup: Result<&RawWindow, &PlatformError>) -> AxMiss {
    match lookup {
        Ok(_) => AxMiss::Keep,
        Err(PlatformError::NotFound | PlatformError::PermissionDenied(_)) => AxMiss::Release,
        Err(_) => AxMiss::Keep,
    }
}

fn retain_ax_error(error: &PlatformError, quartz: Result<(), PlatformError>) -> bool {
    matches!(error, PlatformError::NotFound | PlatformError::Backend(_))
        && !matches!(
            quartz,
            Err(PlatformError::NotFound | PlatformError::PermissionDenied(_))
        )
}

/// What an AX write to the parked window may do, from one fresh look at the Quartz list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreWrite {
    /// The window is showing and nothing covers it: AX may look it up and write.
    Write,
    /// A fullscreen window (the window itself or a same-process stand-in) fills the twin: AX can't
    /// move it and the page's own window may be off-Space. No write; the content is the whole
    /// display.
    WholeDisplay,
    /// The window isn't on screen (another Space, hidden, minimized): AX has no window to write
    /// to, or only the wrong one. No write; report the retained geometry.
    Retain,
}

/// Decide from a fresh read, immediately before an AX write, whether the write is safe. The
/// answer from an earlier read doesn't carry over: Safari can enter fullscreen between the
/// pre-check and the write, and then AX exposes only the title-less stand-in. `displays` are the
/// bounds a fullscreen window may fill: the twin's while the window is parked on it, every active
/// display when restoring after the twin is gone. With none, only the on-screen test applies.
fn pre_write_decision(raw: &RawWindow, list: &[RawWindow], displays: &[RectLogical]) -> PreWrite {
    if fullscreen_on_any_display(raw, list, displays).is_some() {
        PreWrite::WholeDisplay
    } else if !raw.on_screen {
        PreWrite::Retain
    } else {
        PreWrite::Write
    }
}

/// [`PreWrite`] with the window it applies to.
#[derive(Debug)]
enum Plan {
    Write(RawWindow),
    WholeDisplay,
    Retain,
}

/// The one routing for a Quartz read of the parked window, used by every operation (the
/// pre-mode check, the look before the AX write, `geometry`, `restore`). `Err` releases the
/// display and only for a confirmed absence or a revoked permission; a read that failed or timed
/// out is `Retain`.
fn route(
    read: Result<(RawWindow, Vec<RawWindow>), PlatformError>,
    displays: &[RectLogical],
) -> Result<Plan, PlatformError> {
    match read {
        Ok((raw, list)) => Ok(match pre_write_decision(&raw, &list, displays) {
            PreWrite::Write => Plan::Write(raw),
            PreWrite::WholeDisplay => Plan::WholeDisplay,
            PreWrite::Retain => Plan::Retain,
        }),
        Err(error) => match ax_miss_decision(Err(&error)) {
            AxMiss::Release => Err(error),
            AxMiss::Keep => Ok(Plan::Retain),
        },
    }
}

/// How an AX step on the parked window (a frame write, or a frame read) ended.
#[derive(Debug)]
enum AxStep {
    /// The window has this frame now.
    Frame(RectLogical),
    /// AX has no window for it (another Space, a stale element, the strict matching found none).
    Missing,
    /// The late re-check said not to write; nothing (more) was written.
    Withheld,
}

/// What `resize` and `geometry` report once an AX step is settled.
#[derive(Debug, PartialEq)]
enum Settled {
    /// The window's frame, from AX.
    Frame(RectLogical),
    /// A fullscreen window fills the twin: the content is the whole display.
    WholeDisplay,
    /// No AX read or write: retained geometry, or a fresh normal Quartz frame replacing it.
    Retained(Option<RectLogical>),
}

/// Settle an AX step against a fresh read. The race between the plan and the write can't be
/// eliminated (the window can go fullscreen in the instant after the last check), so bound its
/// consequence: a step that failed, was refused, found no window or was withheld never aborts by
/// itself. `replan` re-reads Quartz, and then
/// - newly fullscreen is `WholeDisplay`;
/// - hidden, not on screen, or an inconclusive read is `Retained`;
/// - a window still showing normally, after a *failed* step, is that failure (the existing error
///   path); after a miss or a withheld write, `Retained` at the window's frame.
///
/// A step that took is `Frame` without a re-read. The display is released (`Err`) only for a
/// confirmed `NotFound`, from the step or the re-read, or a revoked permission.
fn settle(
    step: Result<AxStep, PlatformError>,
    replan: impl FnOnce() -> Result<Plan, PlatformError>,
) -> Result<Settled, PlatformError> {
    let failure = match step {
        Ok(AxStep::Frame(frame)) => return Ok(Settled::Frame(frame)),
        Ok(AxStep::Missing | AxStep::Withheld) => None,
        Err(error @ PlatformError::PermissionDenied(_)) => return Err(error),
        Err(error) => Some(error),
    };
    match replan()? {
        Plan::WholeDisplay => Ok(Settled::WholeDisplay),
        Plan::Retain => Ok(Settled::Retained(None)),
        Plan::Write(raw) => match failure {
            None => Ok(Settled::Retained(Some(raw.frame))),
            Some(error) => Err(error),
        },
    }
}

/// The AX window of `raw` by the twin's matching (a fresh AX deadline). A miss is an error here:
/// callers that need the window (parking it, restoring it) can't go on without it.
fn ax_required(raw: &RawWindow) -> Result<AxWindow, PlatformError> {
    match AxWindow::lookup(raw, Instant::now() + AX_WAIT)? {
        AxLookup::Found(ax) => Ok(ax),
        AxLookup::Missing(why) => {
            tracing::debug!(reason = %why, "AX window not found");
            Err(PlatformError::NotFound)
        }
    }
}

/// Move the parked window `raw` to `target` with AX. `raw` is the Quartz window from the read that
/// permitted the write. The AX lookup can take up to `AX_WAIT`, so `guard` (a fresh Quartz read
/// and [`pre_write_decision`]) is asked again right after it and before each frame write.
fn ax_move(
    raw: &RawWindow,
    target: RectLogical,
    guard: &mut dyn FnMut() -> bool,
) -> Result<AxStep, PlatformError> {
    let ax = match AxWindow::lookup(raw, Instant::now() + AX_WAIT) {
        Ok(AxLookup::Found(ax)) => ax,
        Ok(AxLookup::Missing(_)) | Err(PlatformError::NotFound) => return Ok(AxStep::Missing),
        Err(error) => return Err(error),
    };
    match ax.restore_guarded(target, guard) {
        Ok(true) => match ax.frame() {
            Ok(frame) => Ok(AxStep::Frame(frame)),
            Err(PlatformError::NotFound) => Ok(AxStep::Missing),
            Err(error) => Err(error),
        },
        Ok(false) => Ok(AxStep::Withheld),
        Err(PlatformError::NotFound) => Ok(AxStep::Missing),
        Err(error) => Err(error),
    }
}

/// Read the parked window's frame from AX (no write, so no guard).
fn ax_read(raw: &RawWindow) -> Result<AxStep, PlatformError> {
    match AxWindow::lookup(raw, Instant::now() + AX_WAIT) {
        Ok(AxLookup::Found(ax)) => match ax.frame() {
            Ok(frame) => Ok(AxStep::Frame(frame)),
            Err(PlatformError::NotFound) => Ok(AxStep::Missing),
            Err(error) => Err(error),
        },
        Ok(AxLookup::Missing(_)) | Err(PlatformError::NotFound) => Ok(AxStep::Missing),
        Err(error) => Err(error),
    }
}

/// The content rectangle (device pixels) of a window whose frame we couldn't read this time: its
/// last known frame clipped to the twin as it is now (the mode may have changed since). With no
/// frame, or one that no longer touches the twin, the whole twin: capture shows more, never less.
fn kept_content(
    frame: Option<RectLogical>,
    bounds: RectLogical,
    pixels: PixelSize,
) -> Result<PixelRect, PlatformError> {
    frame
        .and_then(|frame| content(frame, bounds, pixels).ok())
        .map_or_else(|| content(bounds, bounds, pixels), Ok)
}

/// M2 parking. Only numeric display handles cross the main-thread boundary.
/// All WindowParking calls must run on a worker thread, never the AppKit main thread:
/// the serial state mutex spans bounded mode/placement waits that need the main queue.
#[derive(Debug)]
pub struct MacTwinParking {
    // One serial lock across native waits; a command worker would be needed for concurrency.
    state: Mutex<TwinState>,
}

#[derive(Debug)]
struct TwinState {
    journal: PathBuf,
    entries: BTreeMap<WindowId, Entry>,
    /// Imported entries are counted only on the first recovery pass, never at shutdown.
    startup_entries: BTreeSet<WindowId>,
    displays: BTreeMap<WindowId, VirtualDisplay>,
    /// Each parked window's last frame AX reported, for when AX has no window for it.
    last: BTreeMap<WindowId, RectLogical>,
    last_fullscreen: BTreeMap<WindowId, bool>,
    /// Last successful measurement on any twin; a failed or timed-out read is best effort.
    last_inset: f64,
    query: WindowQuery,
    /// The bounds of the active displays ([`active_display_bounds`]; a test stubs it).
    probe: fn() -> Result<Vec<RectLogical>, PlatformError>,
    recovery_probe: fn() -> Result<Vec<RectLogical>, PlatformError>,
}

impl MacTwinParking {
    /// Validate every private class, selector and observed macOS 27 ABI before doing anything;
    /// the caller can report an M1 fallback if the API is absent or has changed.
    pub fn new(journal: PathBuf) -> Result<MacTwinParking, PlatformError> {
        classes()?;
        let entries = read_journal(&journal)?;
        Ok(Self {
            state: Mutex::new(TwinState {
                journal,
                startup_entries: entries.keys().copied().collect(),
                entries,
                displays: BTreeMap::new(),
                last: BTreeMap::new(),
                last_fullscreen: BTreeMap::new(),
                last_inset: 0.0,
                query: WindowQuery::new()?,
                probe: active_display_bounds,
                recovery_probe: real_display_bounds,
            }),
        })
    }

    fn state(&self) -> Result<MutexGuard<'_, TwinState>, PlatformError> {
        if MainThreadMarker::new().is_some() {
            return Err(PlatformError::Unsupported(
                "MacTwinParking must be called from a worker thread",
            ));
        }
        self.state.lock().map_err(|_| {
            PlatformError::Backend("twin parking state poisoned; journal retained".into())
        })
    }
}

impl TwinState {
    fn measure_inset(&mut self, id: DisplayId, expected: SizeLogical) -> f64 {
        let (measured, tries) = measure_top_inset(id, expected);
        let inset = remember_top_inset(&mut self.last_inset, measured);
        tracing::debug!(
            display = id.0,
            inset,
            fallback = measured.is_none(),
            tries,
            "twin top inset measurement completed"
        );
        inset
    }

    /// Metadata-only refusal before the resize abort path, journal writes or native calls.
    fn resize_mode(&self, window: WindowId, mode: Mode) -> Result<(Mode, f64), PlatformError> {
        let inset = self
            .displays
            .get(&window)
            .ok_or(PlatformError::NotFound)?
            .inset;
        Ok((mode.with_top_inset(inset)?, inset))
    }

    fn resize_mode_for_plan(
        &self,
        window: WindowId,
        mode: Mode,
        fullscreen: bool,
    ) -> Result<(Mode, f64), PlatformError> {
        if fullscreen {
            Ok((mode, 0.0))
        } else {
            self.resize_mode(window, mode)
        }
    }

    /// The parked window and the whole Quartz list it came from (one WindowServer round trip).
    /// `NotFound`: the window is gone (the list includes windows on every Space).
    fn quartz(&self, window: WindowId) -> Result<(RawWindow, Vec<RawWindow>), PlatformError> {
        self.pick(window, self.query.list(true)?)
    }

    /// [`TwinState::quartz`] whose WindowServer wait ends by `deadline`.
    fn quartz_until(
        &self,
        window: WindowId,
        deadline: Instant,
    ) -> Result<(RawWindow, Vec<RawWindow>), PlatformError> {
        self.pick(window, self.query.list_until(true, deadline)?)
    }

    fn pick(
        &self,
        window: WindowId,
        list: Vec<RawWindow>,
    ) -> Result<(RawWindow, Vec<RawWindow>), PlatformError> {
        let pid = self.entries.get(&window).map(|entry| entry.pid);
        let raw = list
            .iter()
            .find(|raw| raw.id == window && pid.is_none_or(|pid| raw.pid == pid))
            .cloned()
            .ok_or(PlatformError::NotFound)?;
        Ok((raw, list))
    }

    /// One fresh look at the parked window and what an operation may do with it ([`route`]). `Err`
    /// means release the display: the window is confirmed gone, or Accessibility was revoked. A
    /// Quartz read that fails or times out is `Plan::Retain`.
    fn plan(&self, window: WindowId, displays: &[RectLogical]) -> Result<Plan, PlatformError> {
        route(self.quartz(window), displays)
    }

    fn observed_plan(
        &mut self,
        window: WindowId,
        displays: &[RectLogical],
    ) -> Result<Plan, PlatformError> {
        let plan = self.plan(window, displays)?;
        // Commit conclusive Quartz geometry before AX or display waits can lose visibility.
        match &plan {
            Plan::Write(raw) => {
                self.last.insert(window, raw.frame);
                self.last_fullscreen.insert(window, false);
            }
            Plan::WholeDisplay => {
                self.last_fullscreen.insert(window, true);
            }
            Plan::Retain => {}
        }
        Ok(plan)
    }

    /// Whether an AX write to the window may go ahead right now: a fresh read, over every active
    /// display (a window being restored may be fullscreen anywhere), says `Write`. An unreadable
    /// display list or Quartz read says no.
    fn write_allowed(&self, window: WindowId) -> bool {
        match (self.probe)() {
            Ok(displays) => matches!(self.plan(window, &displays), Ok(Plan::Write(_))),
            Err(_) => false,
        }
    }

    /// The window's fullscreen state on any active display, independent of a twin being owned:
    /// the window as Quartz shows it, and its [`Gate`]. For the initial `park` (no twin yet) and
    /// for restoring (after a release or a restart, no twin). `NotFound`: the window is gone.
    fn gate(&self, window: WindowId) -> Result<(Gate, RawWindow), PlatformError> {
        let (raw, list) = self.quartz(window)?;
        let displays = (self.probe)()?;
        Ok((fullscreen_gate(&raw, &list, &displays), raw))
    }

    /// Run one AX step on the parked window around fresh reads of Quartz. First a [`Plan`]: a
    /// window that is fullscreen on the twin or not showing is settled without touching AX. Else
    /// `ax` runs with its `raw` window and a guard that re-reads Quartz; it asks the guard after
    /// the AX lookup and before each frame write. However it ends, [`settle`] re-reads once more:
    /// a failed or refused step never aborts by itself.
    fn guarded_ax(
        &mut self,
        window: WindowId,
        displays: &[RectLogical],
        ax: impl FnOnce(&RawWindow, &mut dyn FnMut() -> bool) -> Result<AxStep, PlatformError>,
    ) -> Result<Settled, PlatformError> {
        let raw = match self.observed_plan(window, displays)? {
            Plan::Write(raw) => raw,
            Plan::WholeDisplay => return Ok(Settled::WholeDisplay),
            Plan::Retain => return Ok(Settled::Retained(None)),
        };
        let mut guard = || matches!(self.observed_plan(window, displays), Ok(Plan::Write(_)));
        let step = ax(&raw, &mut guard);
        settle(step, || self.observed_plan(window, displays))
    }

    /// The parked window's geometry for what `guarded_ax` settled on.
    fn settled(&mut self, window: WindowId, settled: Settled) -> Result<Parked, PlatformError> {
        self.settled_with_metrics(window, settled, self.twin_metrics(window)?)
    }

    fn settled_with_metrics(
        &mut self,
        window: WindowId,
        settled: Settled,
        (display, bounds, pixels): (DisplayId, RectLogical, PixelSize),
    ) -> Result<Parked, PlatformError> {
        let retained = matches!(settled, Settled::Retained(_));
        let (frame, fullscreen) = match settled {
            Settled::Frame(frame) => {
                self.last.insert(window, frame);
                (Some(frame), bounds_equal(frame, bounds))
            }
            Settled::WholeDisplay => (Some(bounds), true),
            // A hint is a fresh normal Quartz frame after an AX miss, not cached geometry.
            Settled::Retained(Some(frame)) => {
                self.last.insert(window, frame);
                self.last_fullscreen.insert(window, false);
                (Some(frame), false)
            }
            Settled::Retained(None) => (
                self.last.get(&window).copied(),
                self.last_fullscreen.get(&window).copied().unwrap_or(false),
            ),
        };
        if !retained {
            self.last_fullscreen.insert(window, fullscreen);
        }
        Ok(Parked {
            fullscreen,
            window,
            kind: ParkingKind::Twin,
            display,
            content: if retained {
                kept_content(if fullscreen { None } else { frame }, bounds, pixels)?
            } else {
                content(frame.unwrap_or(bounds), bounds, pixels)?
            },
        })
    }

    fn resized_fullscreen_with_metrics(
        &mut self,
        window: WindowId,
        metrics: (DisplayId, RectLogical, PixelSize),
    ) -> Result<Parked, PlatformError> {
        // Native mode/placement waits can span a Space transition; use the final twin bounds.
        let settled = settle(Ok(AxStep::Missing), || {
            self.observed_plan(window, &[metrics.1])
        })?;
        self.settled_with_metrics(window, settled, metrics)
    }

    /// The twin's bounds now. `NotFound`: the twin display is gone.
    fn twin_bounds(&self, window: WindowId) -> Result<RectLogical, PlatformError> {
        let display = self.displays.get(&window).ok_or(PlatformError::NotFound)?;
        if !active_displays()?.contains(&display.id.0) {
            return Err(PlatformError::NotFound);
        }
        display_frame(display.id.0).map_err(|_| PlatformError::Timeout)
    }

    /// The twin's display, bounds and pixel size.
    fn twin_metrics(
        &self,
        window: WindowId,
    ) -> Result<(DisplayId, RectLogical, PixelSize), PlatformError> {
        let display = self.displays.get(&window).ok_or(PlatformError::NotFound)?;
        if !active_displays()?.contains(&display.id.0) {
            return Err(PlatformError::NotFound);
        }
        let bounds = display_frame(display.id.0).map_err(|_| PlatformError::Timeout)?;
        let mode = CGDisplayCopyDisplayMode(display.id.0).ok_or(PlatformError::Timeout)?;
        let pixels = PixelSize::new(
            u32::try_from(CGDisplayMode::pixel_width(Some(&mode)))
                .map_err(|_| PlatformError::Backend("twin pixel width overflow".into()))?,
            u32::try_from(CGDisplayMode::pixel_height(Some(&mode)))
                .map_err(|_| PlatformError::Backend("twin pixel height overflow".into()))?,
        );
        Ok((display.id, bounds, pixels))
    }

    fn remove_entry(&mut self, window: WindowId) -> Result<(), PlatformError> {
        let mut entries = self.entries.clone();
        entries.remove(&window);
        write_journal(&self.journal, &entries)?;
        self.entries = entries;
        Ok(())
    }

    fn fullscreen_observation(
        &self,
        window: WindowId,
        deadline: Instant,
        displays: &[RectLogical],
    ) -> Result<(RawWindow, Gate), PlatformError> {
        let (raw, list) = self.quartz_until(window, deadline)?;
        let gate = fullscreen_gate(&raw, &list, displays);
        Ok((raw, gate))
    }

    fn fullscreen_press_needed(
        &self,
        window: WindowId,
        desired: bool,
        deadline: Instant,
        displays: &[RectLogical],
    ) -> Result<bool, PlatformError> {
        let (raw, gate) = self.fullscreen_observation(window, deadline, displays)?;
        if !fullscreen_press_needed(&raw, gate != Gate::Clear, desired, deadline)? {
            return Ok(false);
        }
        if gate == Gate::StandIn {
            return Err(PlatformError::Unsupported(
                "fullscreen stand-in has no button",
            ));
        }
        Ok(true)
    }

    fn set_fullscreen(
        &self,
        window: WindowId,
        desired: bool,
        displays: &[RectLogical],
    ) -> Result<(), PlatformError> {
        let deadline = Instant::now() + FULLSCREEN_WAIT;
        require_accessibility()?;
        ensure_fullscreen_with(
            desired,
            || {
                let (raw, gate) = self.fullscreen_observation(window, deadline, displays)?;
                Ok((raw, gate != Gate::Clear))
            },
            |observed| match AxWindow::lookup(observed, deadline)? {
                AxLookup::Found(ax) => ax.press_fullscreen_button(|| {
                    self.fullscreen_press_needed(window, desired, deadline, displays)
                }),
                AxLookup::Missing(_) => Ok(false),
            },
            || fullscreen_pause(deadline),
        )
    }

    fn restore_frame(
        &self,
        window: WindowId,
        already_home: &mut bool,
    ) -> Result<bool, PlatformError> {
        let Some(entry) = self.entries.get(&window) else {
            return Ok(false);
        };
        // Whether the window is fullscreen on any display, not only on a twin we still own: after
        // a release or a restart there is no twin, and macOS has moved a fullscreen Space to a
        // physical display. The journal stays until a restore succeeds, so `recover` retries.
        let (raw, list) = match self.quartz(window) {
            Ok(found) => found,
            Err(PlatformError::NotFound) => return Ok(false),
            Err(error) => return Err(error),
        };
        if same_frame(raw.frame, entry.frame) {
            *already_home = true;
            return Ok(true);
        }
        let gate = fullscreen_gate(&raw, &list, &(self.probe)()?);
        require_accessibility()?;
        write_journal(&self.journal, &self.entries)?;
        match gate {
            Gate::Clear => {}
            Gate::StandIn => {
                // WebKit's fullscreen window covers a display and the window we parked is
                // off-Space. The stand-in has no button and its frame isn't the one the journal
                // restores: writing it through AX would move the wrong window. Esc in the app
                // ends it; the next `recover` retries.
                return Err(PlatformError::Backend(
                    "a fullscreen stand-in window covers a display; not moved, journal retained"
                        .into(),
                ));
            }
            Gate::Itself => self.set_fullscreen(window, false, &(self.probe)()?)?,
        }
        // A fresh read right before the AX write: leaving fullscreen changes the window, and
        // nothing may be written through AX while a window fills a display or this one isn't
        // showing. `restore` releases the display afterwards and keeps the journal on an error.
        let displays = (self.probe)()?;
        let raw = match self.plan(window, &displays) {
            Ok(Plan::Write(raw)) => raw,
            Ok(Plan::WholeDisplay | Plan::Retain) => {
                return Err(PlatformError::Backend(
                    "parked window is fullscreen or not showing; not moved, journal retained"
                        .into(),
                ));
            }
            Err(PlatformError::NotFound) => return Ok(false),
            Err(error) => return Err(error),
        };
        let ax = ax_required(&raw)?;
        // The AX lookup can take up to two seconds: ask again after it and before each write,
        // the retry included.
        let withheld = || {
            PlatformError::Backend(
                "parked window went fullscreen or left the screen; not moved, journal retained"
                    .into(),
            )
        };
        let mut allowed = || self.write_allowed(window);
        if !ax.restore_guarded(entry.frame, &mut allowed)? {
            return Err(withheld());
        }
        let mut actual = ax.frame()?;
        if (actual.size.width - entry.frame.size.width).abs() > 2.0
            || (actual.size.height - entry.frame.size.height).abs() > 2.0
        {
            // AXSize was written on the twin first and may have been clamped there.
            if !allowed() {
                return Err(withheld());
            }
            ax.resize(entry.frame.size)?;
            actual = ax.frame()?;
        }
        if !same_frame(actual, entry.frame) {
            return Err(PlatformError::Backend(
                "window refused original frame; journal retained".into(),
            ));
        }
        Ok(true)
    }

    fn restore(&mut self, window: WindowId) -> Result<bool, PlatformError> {
        let mut already_home = false;
        let restored = self.restore_with(
            window,
            |state, window| state.restore_frame(window, &mut already_home),
            VirtualDisplay::release,
        )?;
        if already_home {
            tracing::info!("parked window already at its original frame; journal entry removed");
        }
        Ok(restored)
    }

    fn restore_with(
        &mut self,
        window: WindowId,
        restore_frame: impl FnOnce(&Self, WindowId) -> Result<bool, PlatformError>,
        release: impl FnOnce(VirtualDisplay) -> Result<(), PlatformError>,
    ) -> Result<bool, PlatformError> {
        self.last.remove(&window);
        self.last_fullscreen.remove(&window);
        if !self.entries.contains_key(&window) {
            if let Some(display) = self.displays.remove(&window) {
                release(display)?;
            }
            return Ok(false);
        }
        let restored = restore_frame(self, window);
        // Release even if AX restoration failed or permission was revoked; retain the journal.
        let released = self.displays.remove(&window).map(release).transpose();
        if let Err(error) = released.as_ref() {
            tracing::warn!(%error, "twin release failed; cleanup queued on main thread, journal retained");
        }
        let restored = restored?;
        released?;
        self.remove_entry(window)?;
        Ok(restored)
    }

    fn window_on_real_display(&self, window: WindowId) -> Result<RecoveryLocation, PlatformError> {
        let (raw, _) = match self.quartz(window) {
            Ok(found) => found,
            Err(PlatformError::NotFound) => return Ok(RecoveryLocation::Gone),
            Err(error) => return Err(error),
        };
        if !raw.on_screen || !valid_frame(raw.frame) {
            return Ok(RecoveryLocation::Elsewhere);
        }
        // A failed display query (even NotFound) never means the window is gone.
        Ok(if on_real_display(&raw, &(self.recovery_probe)()?) {
            RecoveryLocation::Real
        } else {
            RecoveryLocation::Elsewhere
        })
    }

    /// Startup-only acceptance, before AX and again after a failed restore that may have moved
    /// the window home with a clamped size. An inconclusive read is never proof of restoration.
    fn accept_recovered_with(
        &mut self,
        window: WindowId,
        observed: Result<RecoveryLocation, PlatformError>,
    ) -> Result<Option<bool>, PlatformError> {
        match observed {
            Ok(RecoveryLocation::Real) => {
                self.remove_entry(window)?;
                tracing::info!(
                    window = window.0,
                    "parked window is on a real display; journal entry removed"
                );
                Ok(Some(true))
            }
            Ok(RecoveryLocation::Gone) => {
                self.remove_entry(window)?;
                Ok(Some(false))
            }
            Ok(RecoveryLocation::Elsewhere) | Err(_) => Ok(None),
        }
    }

    fn recover_startup_with(
        &mut self,
        window: WindowId,
        restore: &mut impl FnMut(&mut Self, WindowId) -> Result<bool, PlatformError>,
        observe: &mut impl FnMut(&Self, WindowId) -> Result<RecoveryLocation, PlatformError>,
    ) -> Result<bool, PlatformError> {
        let observed = observe(self, window);
        if let Some(restored) = self.accept_recovered_with(window, observed)? {
            return Ok(restored);
        }
        let error = match restore(self, window) {
            Ok(restored) => return Ok(restored),
            Err(error) => error,
        };
        let observed = observe(self, window);
        if let Some(restored) = self.accept_recovered_with(window, observed)? {
            return Ok(restored);
        }
        let Some(entry) = self.entries.get(&window) else {
            return Err(error);
        };
        let failures = entry.failed_starts.saturating_add(1);
        let mut entries = self.entries.clone();
        if failures >= MAX_FAILED_STARTS {
            entries.remove(&window);
        } else if let Some(entry) = entries.get_mut(&window) {
            entry.failed_starts = failures;
        }
        // Publish count or retirement durably before memory advances or the drop warning.
        write_journal(&self.journal, &entries)?;
        self.entries = entries;
        if failures >= MAX_FAILED_STARTS {
            tracing::warn!(
                window = window.0,
                "parked window recovery failed on three starts; journal entry dropped"
            );
            Ok(false) // Retirement is not a claim that the window was restored.
        } else {
            Err(error)
        }
    }

    fn recover_with(
        &mut self,
        mut restore: impl FnMut(&mut Self, WindowId) -> Result<bool, PlatformError>,
        mut observe: impl FnMut(&Self, WindowId) -> Result<RecoveryLocation, PlatformError>,
    ) -> Result<Vec<WindowId>, PlatformError> {
        let startup = std::mem::take(&mut self.startup_entries);
        let mut restored = Vec::new();
        let mut first_error = None;
        let windows: BTreeSet<_> = self
            .entries
            .keys()
            .chain(self.displays.keys())
            .copied()
            .collect();
        for window in windows {
            let result = if startup.contains(&window) && !self.displays.contains_key(&window) {
                self.recover_startup_with(window, &mut restore, &mut observe)
            } else {
                restore(self, window)
            };
            match result {
                Ok(true) => restored.push(window),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "twin recovery failed; continuing with remaining windows");
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(restored), Err)
    }

    fn abort(&mut self, window: WindowId, error: PlatformError) -> PlatformError {
        self.abort_with(window, error, |state, window| {
            state.restore(window).map(|_| ())
        })
    }

    fn abort_with(
        &mut self,
        window: WindowId,
        error: PlatformError,
        rollback: impl FnOnce(&mut Self, WindowId) -> Result<(), PlatformError>,
    ) -> PlatformError {
        if matches!(error, PlatformError::NotFound | PlatformError::Backend(_))
            && retain_ax_error(&error, self.quartz(window).map(|_| ()))
        {
            return error;
        }
        match rollback(self, window) {
            Ok(_) => error,
            Err(rollback) => {
                tracing::warn!(%rollback, "twin rollback failed; display released or cleanup queued, journal retained");
                if let PlatformError::PermissionDenied(permission) = rollback {
                    PlatformError::PermissionDenied(permission)
                } else {
                    error
                }
            }
        }
    }

    fn finish_geometry(
        &mut self,
        window: WindowId,
        result: Result<Parked, PlatformError>,
    ) -> Result<Parked, PlatformError> {
        result.or_else(|error| {
            if matches!(error, PlatformError::NotFound | PlatformError::Backend(_))
                && retain_ax_error(&error, self.quartz(window).map(|_| ()))
            {
                let twin = self.twin_bounds(window)?;
                let settled = match self.observed_plan(window, &[twin]) {
                    Ok(Plan::WholeDisplay) => Settled::WholeDisplay,
                    Ok(Plan::Write(raw)) => Settled::Frame(raw.frame),
                    _ => Settled::Retained(None),
                };
                return self
                    .settled(window, settled)
                    .or_else(|_| self.settled(window, Settled::Retained(None)));
            }
            Err(self.abort(window, error))
        })
    }

    /// Native park and injected tests share the fresh plan, write marker and failure routing.
    fn park_sequence_with<A>(
        &mut self,
        window: WindowId,
        mut attempt: ParkAttempt,
        prepare: impl FnOnce(&mut Self) -> Result<(RectLogical, RectLogical), PlatformError>,
        ax: (
            impl FnOnce(&RawWindow) -> Result<A, PlatformError>,
            impl FnOnce(A, RectLogical, &mut dyn FnMut() -> bool) -> Result<RectLogical, PlatformError>,
        ),
        finish: impl FnOnce(&mut Self) -> Result<Parked, PlatformError>,
        cleanup: (
            impl FnOnce(VirtualDisplay) -> Result<(), PlatformError>,
            impl FnOnce(&mut Self, WindowId, PlatformError) -> PlatformError,
        ),
    ) -> Result<Parked, PlatformError> {
        let result = (|| {
            let (bounds, target) = prepare(self)?;
            // No geometry can be retained yet: a fresh fullscreen/off-Space refusal is cleanup.
            let raw = match self.observed_plan(window, &[bounds])? {
                Plan::Write(raw) => raw,
                Plan::WholeDisplay | Plan::Retain => {
                    return Err(PlatformError::Backend(
                        "window is fullscreen or not showing; not parked".into(),
                    ));
                }
            };
            let window_ax = (ax.0)(&raw)?;
            // The lookup can take two seconds: re-read after it and before each frame write.
            let mut allowed =
                || matches!(self.observed_plan(window, &[bounds]), Ok(Plan::Write(_)));
            // AXSize can be written before restore_guarded's later guard refuses AXPosition.
            attempt.write_attempted = true;
            let frame = (ax.1)(window_ax, target, &mut allowed)?;
            self.last.insert(window, frame);
            finish(self)
        })();
        result.map_err(|error| {
            if attempt.write_attempted {
                return (cleanup.1)(self, window, error);
            }
            if let Some(display) = self.displays.remove(&window)
                && let Err(cleanup) = (cleanup.0)(display)
            {
                tracing::warn!(%cleanup, "refused park twin cleanup failed; release queued on main thread");
            }
            if attempt.forget_entry
                && let Err(cleanup) = self.remove_entry(window)
            {
                tracing::warn!(%cleanup, "refused park journal cleanup failed; entry retained");
            }
            error
        })
    }

    fn park(&mut self, window: WindowId, mode: Mode) -> Result<Parked, PlatformError> {
        if self.displays.contains_key(&window) {
            return self.resize(window, mode);
        }
        require_accessibility()?;
        // A window that is fullscreen on any display can't be parked: nothing AX does moves it.
        // Itself: press its button, wait, re-read, and park as normal. A stand-in: refuse before
        // any twin exists, so there is nothing to roll back.
        let raw = match self.gate(window)? {
            (Gate::Clear, raw) => raw,
            (Gate::StandIn, _) => {
                return Err(PlatformError::Unsupported(
                    "leave fullscreen before projecting",
                ));
            }
            (Gate::Itself, _) => {
                self.set_fullscreen(window, false, &(self.probe)()?)?;
                match self.gate(window)? {
                    (Gate::Clear, raw) => raw,
                    _ => {
                        return Err(PlatformError::Unsupported(
                            "leave fullscreen before projecting",
                        ));
                    }
                }
            }
        };
        let ax = ax_required(&raw)?;
        let original = ax.frame()?;
        let inserted = !self.entries.contains_key(&window);
        let journaled = self
            .entries
            .entry(window)
            .or_insert(Entry {
                pid: raw.pid,
                frame: original,
                failed_starts: 0,
            })
            .frame;
        self.park_sequence_with(
            window,
            ParkAttempt::new(inserted, original, journaled),
            |state| {
                write_journal(&state.journal, &state.entries)?;
                let display = VirtualDisplay::create(mode)?;
                if let Err(error) = wait_mode(display.id, mode) {
                    if let Err(cleanup) = display.release() {
                        tracing::warn!(%cleanup, "twin setup cleanup queued on main thread");
                    }
                    return Err(error);
                }
                let id = display.id;
                state.displays.insert(window, display);
                let mut placement = place_twin(id)?;
                let measured = state.measure_inset(id, mode.logical());
                // At capacity, keep today's valid zero-inset parking rather than aborting it.
                let (grown, inset) = mode.fitting_top_inset(measured, (mode, 0.0));
                let display = state
                    .displays
                    .get_mut(&window)
                    .ok_or(PlatformError::NotFound)?;
                if grown != mode {
                    display.apply(grown)?;
                    wait_mode(id, grown)?;
                    placement = place_twin(id)?;
                }
                display.inset = inset;
                let target = target_rect(placement.frame, inset, mode.logical());
                Ok((placement.frame, target))
            },
            (ax_required, |ax, target, allowed| {
                if !ax.restore_guarded(target, allowed)? {
                    return Err(PlatformError::Backend(
                        "window went fullscreen or left the screen; not parked".into(),
                    ));
                }
                ax.frame()
            }),
            |state| state.geometry(window),
            (VirtualDisplay::release, Self::abort),
        )
    }

    fn resize(&mut self, window: WindowId, mode: Mode) -> Result<Parked, PlatformError> {
        // Observe the old bounds first; padding refusal still precedes journal/native mutations.
        // Only a fullscreen window filling the old twin bypasses the normal menu reservation.
        let fullscreen = self
            .twin_bounds(window)
            .and_then(|twin| self.observed_plan(window, &[twin]))
            .map(|plan| matches!(plan, Plan::WholeDisplay))
            .map_err(|error| self.abort(window, error))?;
        let (grown, previous_inset) = self.resize_mode_for_plan(window, mode, fullscreen)?;
        let result = (|| {
            require_accessibility()?;
            write_journal(&self.journal, &self.entries)?;
            let display = self.displays.get(&window).ok_or(PlatformError::NotFound)?;
            let id = display.id;
            display.apply(grown)?;
            wait_mode(id, grown)?;
            if fullscreen {
                place_twin(id)?;
                return self.resized_fullscreen_with_metrics(window, self.twin_metrics(window)?);
            }
            let measured = self.measure_inset(id, grown.logical());
            let (corrected, inset) = mode.fitting_top_inset(measured, (grown, previous_inset));
            let display = self
                .displays
                .get_mut(&window)
                .ok_or(PlatformError::NotFound)?;
            // A mode change can move the menu bar. Correct at most once, never chase it in a loop.
            if corrected != grown {
                display.apply(corrected)?;
                wait_mode(id, corrected)?;
            }
            display.inset = inset;
            // Mode changes can alter the arrangement. Re-isolate before moving the window again.
            let placement = place_twin(id)?;
            // The mode change and the placement wait can take a while, and the window can go
            // fullscreen meanwhile: look again right before the write, after the AX lookup, and
            // before each frame write. Then AX exposes only the title-less stand-in, or takes
            // the write on a fullscreen window and refuses it; neither aborts (`settle`).
            let target = target_rect(placement.frame, inset, mode.logical());
            let settled = self.guarded_ax(window, &[placement.frame], |raw, guard| {
                ax_move(raw, target, guard)
            })?;
            self.settled(window, settled)
        })();
        self.finish_geometry(window, result)
    }

    fn geometry(&mut self, window: WindowId) -> Result<Parked, PlatformError> {
        if !self.displays.contains_key(&window) {
            return Err(PlatformError::NotFound);
        }
        require_accessibility()?;
        let twin = self.twin_bounds(window)?;
        let settled = self.guarded_ax(window, &[twin], |raw, _guard| ax_read(raw))?;
        self.settled(window, settled)
    }
}

impl WindowParking for MacTwinParking {
    fn set_fullscreen(&mut self, window: WindowId, fullscreen: bool) -> Result<(), PlatformError> {
        let state = self.state()?;
        if !state.displays.contains_key(&window) {
            return Err(PlatformError::NotFound);
        }
        state.set_fullscreen(window, fullscreen, &[state.twin_bounds(window)?])
    }

    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.state()?.park(window, Mode::new(size, scale)?)
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.state()?.resize(window, Mode::new(size, scale)?)
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        let mut state = self.state()?;
        let result = state.geometry(window);
        state.finish_geometry(window, result)
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        self.state()?.restore(window).map(|_| ())
    }

    fn restore_at(
        &mut self,
        window: WindowId,
        display: DisplayId,
        origin: crosspane_types::geom::PointDevice,
    ) -> Result<(), PlatformError> {
        crate::parking::restore_and_place(
            || self.restore(window),
            || crate::parking::place_restored_window(window, display, origin, true),
        )
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        self.state()?
            .recover_with(TwinState::restore, TwinState::window_on_real_display)
    }
}

impl Drop for MacTwinParking {
    fn drop(&mut self) {
        let state = match self.state.get_mut() {
            Ok(state) => state,
            Err(error) => error.into_inner(),
        };
        for window in state.displays.keys().copied().collect::<Vec<_>>() {
            if let Err(error) = state.restore(window) {
                // Never log titles or typed content. The durable journal remains available to recover().
                tracing::warn!(%error, "twin restoration failed during drop; journal retained");
            }
        }
        // Drop queues main-thread release even when permission was revoked during cleanup.
    }
}

fn same_frame(a: RectLogical, b: RectLogical) -> bool {
    (a.origin.x - b.origin.x).abs() <= 2.0
        && (a.origin.y - b.origin.y).abs() <= 2.0
        && (a.size.width - b.size.width).abs() <= 2.0
        && (a.size.height - b.size.height).abs() <= 2.0
}

fn content(
    frame: RectLogical,
    bounds: RectLogical,
    pixels: PixelSize,
) -> Result<PixelRect, PlatformError> {
    if !valid_frame(frame)
        || !valid_frame(bounds)
        || pixels.width == 0
        || pixels.height == 0
        || pixels.width > i32::MAX as u32
        || pixels.height > i32::MAX as u32
    {
        return Err(PlatformError::Backend("invalid twin geometry".into()));
    }
    let clipped = frame
        .intersection(&bounds)
        .filter(|frame| valid_frame(*frame))
        .ok_or_else(|| PlatformError::Backend("window has no content on its twin".into()))?;
    let scale_x = f64::from(pixels.width) / bounds.size.width;
    let scale_y = f64::from(pixels.height) / bounds.size.height;
    let edges = [
        ((clipped.min_x() - bounds.min_x()) * scale_x)
            .floor()
            .clamp(0.0, f64::from(pixels.width)),
        ((clipped.min_y() - bounds.min_y()) * scale_y)
            .floor()
            .clamp(0.0, f64::from(pixels.height)),
        ((clipped.max_x() - bounds.min_x()) * scale_x)
            .ceil()
            .clamp(0.0, f64::from(pixels.width)),
        ((clipped.max_y() - bounds.min_y()) * scale_y)
            .ceil()
            .clamp(0.0, f64::from(pixels.height)),
    ];
    if edges.iter().any(|v| !v.is_finite()) {
        return Err(PlatformError::Backend("invalid twin pixel geometry".into()));
    }
    Ok(PixelRect::new(
        euclid::Point2D::new(edges[0] as i32, edges[1] as i32),
        euclid::Point2D::new(edges[2] as i32, edges[3] as i32),
    ))
}

fn io_error(error: std::io::Error) -> PlatformError {
    PlatformError::Backend(format!("twin journal: {error}"))
}

fn read_journal(path: &Path) -> Result<BTreeMap<WindowId, Entry>, PlatformError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(io_error(error)),
    };
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(io_error)?;
    parse_journal(&text)
}

fn parse_journal(text: &str) -> Result<BTreeMap<WindowId, Entry>, PlatformError> {
    let invalid = || PlatformError::Backend("invalid twin journal; retained for inspection".into());
    let mut lines = text.lines();
    if lines.next() != Some("crosspane-twin-v1") {
        return Err(invalid());
    }
    let mut entries = BTreeMap::new();
    for line in lines {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() != 6 && parts.len() != 7 {
            return Err(invalid());
        }
        let window = WindowId(parts[0].parse::<u64>().map_err(|_| invalid())?);
        let pid = parts[1].parse::<i32>().map_err(|_| invalid())?;
        let failed_starts = parts
            .get(6)
            .map(|count| count.parse::<u8>().map_err(|_| invalid()))
            .transpose()?
            .unwrap_or(0);
        let values = parts[2..6]
            .iter()
            .map(|v| v.parse::<f64>().map_err(|_| invalid()))
            .collect::<Result<Vec<_>, _>>()?;
        let frame = RectLogical::new(
            PointLogical::new(values[0], values[1]),
            SizeLogical::new(values[2], values[3]),
        );
        if window.0 == 0
            || window.0 > u64::from(u32::MAX)
            || pid <= 0
            || !valid_frame(frame)
            || entries
                .insert(
                    window,
                    Entry {
                        pid,
                        frame,
                        failed_starts,
                    },
                )
                .is_some()
        {
            return Err(invalid());
        }
    }
    Ok(entries)
}

fn journal_line(window: WindowId, entry: &Entry) -> String {
    format!(
        "{} {} {} {} {} {} {}",
        window.0,
        entry.pid,
        entry.frame.origin.x,
        entry.frame.origin.y,
        entry.frame.size.width,
        entry.frame.size.height,
        entry.failed_starts
    )
}

fn write_journal(path: &Path, entries: &BTreeMap<WindowId, Entry>) -> Result<(), PlatformError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| PlatformError::Backend("journal needs a file name".into()))?;
    let mut temporary_name = name.to_os_string();
    temporary_name.push(format!(".{}.pending", std::process::id()));
    let temporary = parent.join(temporary_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(io_error)?;
    let result = (|| {
        writeln!(file, "crosspane-twin-v1").map_err(io_error)?;
        for (window, entry) in entries {
            writeln!(file, "{}", journal_line(*window, entry)).map_err(io_error)?;
        }
        file.sync_all().map_err(io_error)?;
        fs::rename(&temporary, path).map_err(io_error)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(io_error)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[allow(unused_imports)]
    // Standalone GUI driver compiles helpers without the #[test] bodies.
    use crate::windows::next_sleep;

    #[test]
    fn encoding_comparison_fails_closed() {
        assert!(encoding_matches(
            Some(c"@32@0:8I16I20d24"),
            c"@32@0:8I16I20d24"
        ));
        for observed in [
            None,
            Some(c"@40@0:8Q16Q24d32"),
            Some(c"@32@0:8i16i20d24"),
            Some(c"@32@0:8I16I24d24"),
            Some(c"@32@0:8I16I20d24junk"),
        ] {
            assert!(!encoding_matches(observed, c"@32@0:8I16I20d24"));
        }
        assert!(!encoding_matches(Some(c"c24@0:8@16"), c"B24@0:8@16"));
    }

    #[test]
    fn ax_failure_keeps_a_quartz_window_and_releases_only_confirmed_loss() {
        for error in [
            PlatformError::NotFound,
            PlatformError::Backend("AX miss".into()),
        ] {
            assert!(retain_ax_error(&error, Ok(())));
            assert!(retain_ax_error(&error, Err(PlatformError::Timeout)));
            assert!(retain_ax_error(
                &error,
                Err(PlatformError::Backend("Quartz stalled".into()))
            ));
            assert!(!retain_ax_error(&error, Err(PlatformError::NotFound)));
            assert!(!retain_ax_error(
                &error,
                Err(PlatformError::PermissionDenied(
                    crosspane_platform::Permission::Accessibility
                ))
            ));
        }
        assert!(!retain_ax_error(
            &PlatformError::PermissionDenied(crosspane_platform::Permission::Accessibility),
            Ok(())
        ));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn mode_size_and_logical_math() {
        for (size, scale, logical, hidpi) in [
            (
                PixelSize::new(1600, 1200),
                1.0,
                SizeLogical::new(1600.0, 1200.0),
                0,
            ),
            (
                PixelSize::new(1600, 1200),
                2.0,
                SizeLogical::new(800.0, 600.0),
                1,
            ),
            (
                PixelSize::new(1600, 1200),
                1.5,
                SizeLogical::new(800.0, 600.0),
                1,
            ),
            (
                PixelSize::new(1600, 1200),
                3.0,
                SizeLogical::new(800.0, 600.0),
                1,
            ),
            (
                PixelSize::new(1601, 1201),
                1.0,
                SizeLogical::new(1601.0, 1201.0),
                0,
            ),
            (
                PixelSize::new(1601, 1201),
                2.0,
                SizeLogical::new(801.0, 601.0),
                1,
            ),
        ] {
            let mode = Mode::new(size, scale).unwrap();
            let density = if hidpi == 1 { 2 } else { 1 };
            assert_eq!(
                mode.pixels,
                PixelSize::new(mode.width * density, mode.height * density)
            );
            assert!(mode.pixels.width >= size.width && mode.pixels.height >= size.height);
            assert_eq!(mode.logical(), logical);
            assert_eq!(mode.hidpi, hidpi);
        }
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(Mode::new(PixelSize::new(1600, 1200), scale).is_err());
        }
        assert!(Mode::new(PixelSize::new(0, 1200), 2.0).is_err());
        assert!(Mode::new(PixelSize::new(MAX_PIXELS, MAX_PIXELS), 2.0).is_ok());
        for size in [
            PixelSize::new(MAX_PIXELS + 1, 1200),
            PixelSize::new(1600, MAX_PIXELS + 1),
        ] {
            assert!(matches!(
                Mode::new(size, 2.0),
                Err(PlatformError::Unsupported(_))
            ));
        }
        let bounds = RectLogical::new(
            PointLogical::new(1800.0, -600.0),
            SizeLogical::new(800.0, 600.0),
        );
        let pixels = PixelSize::new(1600, 1200);
        let frame = RectLogical::new(
            PointLogical::new(1790.0, -610.0),
            SizeLogical::new(900.0, 700.0),
        );
        assert_eq!(
            content(frame, bounds, pixels).unwrap(),
            PixelRect::new(euclid::Point2D::new(0, 0), euclid::Point2D::new(1600, 1200))
        );
        let frame = RectLogical::new(
            PointLogical::new(1830.0, -580.0),
            SizeLogical::new(200.0, 100.0),
        );
        assert_eq!(
            content(frame, bounds, pixels).unwrap(),
            PixelRect::new(euclid::Point2D::new(60, 40), euclid::Point2D::new(460, 240))
        );
        assert!(
            content(
                RectLogical::new(
                    PointLogical::new(2600.0, -600.0),
                    SizeLogical::new(10.0, 10.0)
                ),
                bounds,
                pixels
            )
            .is_err()
        );
        let real = [RectLogical::new(
            PointLogical::zero(),
            SizeLogical::new(800.0, 600.0),
        )];
        let twin = |x, y| RectLogical::new(PointLogical::new(x, y), SizeLogical::new(400.0, 300.0));
        for (x, y) in [
            (800.0, -300.0),
            (-400.0, -300.0),
            (-400.0, 600.0),
            (800.0, 600.0),
        ] {
            assert!(corner_only(twin(x, y), &real));
        }
        assert!(!corner_only(twin(800.0, 0.0), &real));
        assert!(!corner_only(twin(799.0, -299.0), &real));
        assert!(!corner_only(twin(801.0, -301.0), &real));
        assert!(!corner_only(
            twin(800.0, -300.0),
            &[real[0], twin(800.0, -600.0)]
        ));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn twin_mode_reserves_menu_bar_at_hidpi() {
        let requested = Mode::new(PixelSize::new(3420, 2812), 2.0).unwrap();
        let twin = requested.with_top_inset(30.0).unwrap();
        assert_eq!(requested.logical(), SizeLogical::new(1710.0, 1406.0));
        assert_eq!(twin.logical(), SizeLogical::new(1710.0, 1436.0));
        assert_eq!(twin.pixels, PixelSize::new(3420, 2872));
        assert_eq!(twin.hidpi, requested.hidpi);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn twin_mode_preserves_odd_pixel_rounding() {
        for scale in [1.0, 1.5, 2.0, 3.0] {
            let requested = Mode::new(PixelSize::new(1601, 1201), scale).unwrap();
            let density = if scale > 1.0 { 2 } else { 1 };
            let expected = Mode::new(
                PixelSize::new(requested.pixels.width, requested.pixels.height + 61),
                f64::from(density),
            )
            .unwrap();
            assert_eq!(
                requested.with_top_inset(61.0 / f64::from(density)).unwrap(),
                expected
            );
            assert_eq!(expected.hidpi, requested.hidpi);
        }
    }

    #[test]
    fn top_inset_is_clamped_to_sixty_four_points() {
        for (visible_max_y, expected) in
            [(1010.0, 0.0), (1000.0, 0.0), (970.0, 30.0), (900.0, 64.0)]
        {
            assert_eq!(top_inset(1000.0, visible_max_y), Some(expected));
        }
        assert_eq!(top_inset(-1406.0, -1436.0), Some(30.0));
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(top_inset(value, 0.0), None);
            assert_eq!(top_inset(0.0, value), None);
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn twin_mode_refuses_height_plus_inset_over_pixel_limit() {
        for scale in [1.0, 2.0] {
            let density = if scale > 1.0 { 2 } else { 1 };
            let requested =
                Mode::new(PixelSize::new(MAX_PIXELS, MAX_PIXELS - 60 * density), scale).unwrap();
            assert_eq!(
                requested.with_top_inset(60.0).unwrap().pixels.height,
                MAX_PIXELS
            );
            assert!(matches!(
                requested.with_top_inset(60.5),
                Err(PlatformError::Unsupported(_))
            ));
            let full = Mode::new(PixelSize::new(MAX_PIXELS, MAX_PIXELS), scale).unwrap();
            assert!(matches!(
                full.with_top_inset(1.0),
                Err(PlatformError::Unsupported(_))
            ));
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn target_rectangle_below_inset_has_exact_requested_size() {
        let requested = Mode::new(PixelSize::new(3420, 2812), 2.0).unwrap();
        let twin = requested.with_top_inset(30.0).unwrap();
        let bounds = RectLogical::new(PointLogical::new(1800.0, -1436.0), twin.logical());
        let target = target_rect(bounds, 30.0, requested.logical());
        assert_eq!(target, rect(1800.0, -1406.0, 1710.0, 1406.0));
        assert_eq!(target.max_y(), bounds.max_y());
        assert_eq!(
            content(target, bounds, twin.pixels).unwrap(),
            PixelRect::new(
                euclid::Point2D::new(0, 60),
                euclid::Point2D::new(3420, 2872)
            )
        );
        // An app's smaller actual frame still determines the crop; no requested-size fabrication.
        let actual = rect(target.min_x(), target.min_y(), 850.0, 1376.0);
        assert_eq!(
            content(actual, bounds, twin.pixels).unwrap(),
            PixelRect::new(
                euclid::Point2D::new(0, 60),
                euclid::Point2D::new(1700, 2812)
            )
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn zero_inset_preserves_existing_mode_and_target() {
        for scale in [1.0, 2.0] {
            let requested = Mode::new(PixelSize::new(1601, 1201), scale).unwrap();
            let bounds = RectLogical::new(PointLogical::new(1800.0, -601.0), requested.logical());
            assert_eq!(requested.with_top_inset(0.0).unwrap(), requested);
            assert_eq!(target_rect(bounds, 0.0, requested.logical()), bounds);
        }
    }

    #[test]
    fn inset_read_failure_reuses_last_measurement() {
        let mut last = 0.0;
        assert_eq!(remember_top_inset(&mut last, None), 0.0);
        assert_eq!(remember_top_inset(&mut last, Some(30.0)), 30.0);
        assert_eq!(remember_top_inset(&mut last, None), 30.0);
        // A measurement on another twin supersedes the fallback, including a real zero inset.
        assert_eq!(remember_top_inset(&mut last, Some(0.0)), 0.0);
        assert_eq!(remember_top_inset(&mut last, None), 0.0);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn inset_overflow_refusal_preserves_twin_window_frame_and_journal() {
        let window = WindowId(10);
        let mut state = scripted_state(vec![]);
        let frame = rect(1800.0, -1406.0, 1710.0, 1406.0);
        state.last.insert(window, frame);
        // Numeric metadata only: no native display is created, and this handle is forgotten below.
        state.displays.insert(
            window,
            VirtualDisplay {
                serial: u32::MAX,
                id: DisplayId(99),
                inset: 30.0,
            },
        );
        state.journal = std::env::temp_dir().join(format!(
            "crosspane-twin-inset-refusal-{}-{}.journal",
            std::process::id(),
            crate::clock::now().as_nanos()
        ));
        write_journal(&state.journal, &state.entries).unwrap();
        let journal = fs::read(&state.journal).unwrap();
        let entries = state.entries.clone();
        let requested = Mode::new(PixelSize::new(1710, 8180), 1.0).unwrap();
        assert!(matches!(
            state.resize_mode(window, requested),
            Err(PlatformError::Unsupported(_))
        ));
        let twin = state.displays.get(&window).unwrap();
        assert_eq!(
            (twin.serial, twin.id, twin.inset),
            (u32::MAX, DisplayId(99), 30.0)
        );
        assert_eq!(state.last.get(&window), Some(&frame));
        assert_eq!(state.entries, entries);
        assert_eq!(fs::read(&state.journal).unwrap(), journal);
        assert_eq!(read_journal(&state.journal).unwrap(), entries);
        std::mem::forget(state.displays.remove(&window).unwrap());
        fs::remove_file(&state.journal).unwrap();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn inset_overflow_keeps_valid_park_and_resize_modes() {
        let requested = Mode::new(PixelSize::new(1710, 8180), 1.0).unwrap();
        // Parking has already created a valid twin: an oversized reservation keeps zero-inset geometry.
        assert_eq!(
            requested.fitting_top_inset(30.0, (requested, 0.0)),
            (requested, 0.0)
        );
        // Resize applied the known ten-point inset; a new thirty-point read won't fit.
        let grown = requested.with_top_inset(10.0).unwrap();
        let (corrected, inset) = requested.fitting_top_inset(30.0, (grown, 10.0));
        assert_eq!((corrected, inset), (grown, 10.0));
        let bounds = RectLogical::new(PointLogical::new(1800.0, -8190.0), corrected.logical());
        let target = target_rect(bounds, inset, requested.logical());
        assert_eq!(target, rect(1800.0, -8180.0, 1710.0, 8180.0));
        assert_eq!(target.max_y(), bounds.max_y());
    }

    #[test]
    fn inset_read_accepts_current_screen() {
        let expected = SizeLogical::new(1710.0, 1436.0);
        for inset in [0.0, 30.0] {
            assert_eq!(
                inset_read(Some((expected, inset)), expected, INSET_WAIT),
                InsetRead::Measured(inset)
            );
        }
    }

    #[test]
    fn inset_read_retries_stale_frame() {
        let expected = SizeLogical::new(1710.0, 1436.0);
        for stale in [
            SizeLogical::new(1710.0, 1406.0),
            SizeLogical::new(1700.0, 1436.0),
        ] {
            assert_eq!(
                inset_read(Some((stale, 30.0)), expected, INSET_WAIT),
                InsetRead::Retry(INSET_POLL)
            );
        }
    }

    #[test]
    fn inset_read_retries_missing_screen() {
        let expected = SizeLogical::new(1710.0, 1436.0);
        assert_eq!(
            inset_read(None, expected, INSET_WAIT),
            InsetRead::Retry(INSET_POLL)
        );
        let remaining = Duration::from_millis(20);
        assert_eq!(
            inset_read(None, expected, remaining),
            InsetRead::Retry(remaining)
        );
    }

    #[test]
    fn inset_read_falls_back_when_budget_exhausted() {
        let expected = SizeLogical::new(1710.0, 1436.0);
        for observed in [
            None,
            Some((expected, 30.0)),
            Some((SizeLogical::new(1710.0, 1406.0), 30.0)),
        ] {
            assert_eq!(
                inset_read(observed, expected, Duration::ZERO),
                InsetRead::Fallback
            );
        }
    }

    #[test]
    fn visible_area_window_is_not_fullscreen_on_enlarged_twin() {
        let bounds = rect(1800.0, -1436.0, 1710.0, 1436.0);
        let window = RawWindow::fixture(
            10,
            500,
            target_rect(bounds, 30.0, SizeLogical::new(1710.0, 1406.0)),
            true,
        );
        assert_eq!(
            fullscreen_gate(&window, std::slice::from_ref(&window), &[bounds]),
            Gate::Clear
        );
        let fullscreen = RawWindow::fixture(10, 500, bounds, true);
        assert_eq!(
            fullscreen_gate(&fullscreen, std::slice::from_ref(&fullscreen), &[bounds]),
            Gate::Itself
        );
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> RectLogical {
        RectLogical::new(PointLogical::new(x, y), SizeLogical::new(w, h))
    }

    /// The built-in display.
    fn built_in() -> RectLogical {
        rect(0.0, 0.0, 1800.0, 1169.0)
    }

    #[test]
    fn fullscreen_on_any_display_counts_the_window_or_a_same_process_stand_in() {
        let twin = rect(1800.0, -1406.0, 1710.0, 1406.0);
        let displays = [built_in(), twin];
        // The parked window under the twin's menu bar: not fullscreen.
        let parked = RawWindow::fixture(10, 500, rect(1800.0, -1376.0, 1710.0, 1376.0), true);
        // WebKit's fullscreen window: same process, fills the twin.
        let stand_in = RawWindow::fixture(11, 500, twin, true);
        let other_process = RawWindow::fixture(12, 600, twin, true);
        let off_screen = RawWindow::fixture(13, 500, twin, false);
        // A window at the built-in display's size, but not at a display's bounds.
        let nearly = RawWindow::fixture(14, 500, rect(0.0, 38.0, 1800.0, 1131.0), true);

        // Nothing fills a display.
        assert_eq!(
            fullscreen_on_any_display(&parked, &[parked.clone(), nearly.clone()], &displays),
            None
        );
        // The window itself goes fullscreen (the green button), on the twin or on any other
        // display, owned or not.
        let itself = RawWindow::fixture(10, 500, twin, true);
        assert_eq!(
            fullscreen_on_any_display(&itself, std::slice::from_ref(&itself), &displays),
            Some(WindowId(10))
        );
        let on_built_in = RawWindow::fixture(10, 500, built_in(), true);
        assert_eq!(
            fullscreen_on_any_display(&on_built_in, std::slice::from_ref(&on_built_in), &displays),
            Some(WindowId(10))
        );
        // A stand-in of the same process counts, wherever it is, and it is the stand-in that is
        // reported.
        assert_eq!(
            fullscreen_on_any_display(&parked, &[parked.clone(), stand_in.clone()], &displays),
            Some(WindowId(11))
        );
        let stand_in_built_in = RawWindow::fixture(11, 500, built_in(), true);
        assert_eq!(
            fullscreen_on_any_display(&parked, &[parked.clone(), stand_in_built_in], &displays),
            Some(WindowId(11))
        );
        // The window itself wins over a stand-in.
        assert_eq!(
            fullscreen_on_any_display(&itself, &[itself.clone(), stand_in.clone()], &displays),
            Some(WindowId(10))
        );
        // Another process's fullscreen window is not ours.
        assert_eq!(
            fullscreen_on_any_display(&parked, &[parked.clone(), other_process], &displays),
            None
        );
        // A window that isn't on screen (the page window on its old Space, ours hidden) doesn't
        // count, even with a display's bounds.
        assert_eq!(
            fullscreen_on_any_display(&parked, &[parked.clone(), off_screen], &displays),
            None
        );
        let hidden_itself = RawWindow::fixture(10, 500, twin, false);
        assert_eq!(
            fullscreen_on_any_display(
                &hidden_itself,
                std::slice::from_ref(&hidden_itself),
                &displays
            ),
            None
        );
        // No display matching: a bounds mismatch, a display that isn't listed, no displays.
        let other_size = RawWindow::fixture(15, 500, rect(1800.0, -1406.0, 1710.0, 1400.0), true);
        assert_eq!(
            fullscreen_on_any_display(&parked, &[parked.clone(), other_size], &displays),
            None
        );
        assert_eq!(
            fullscreen_on_any_display(&itself, std::slice::from_ref(&itself), &[built_in()]),
            None
        );
        assert_eq!(
            fullscreen_on_any_display(&itself, std::slice::from_ref(&itself), &[]),
            None
        );
    }

    #[test]
    fn the_gate_names_who_fills_a_display() {
        let twin = rect(1800.0, -1406.0, 1710.0, 1406.0);
        let displays = [built_in(), twin];
        let page = RawWindow::fixture(10, 500, rect(0.0, 38.0, 1800.0, 1131.0), true);
        let hidden = RawWindow::fixture(10, 500, rect(0.0, 38.0, 1800.0, 1131.0), false);
        let itself = RawWindow::fixture(10, 500, built_in(), true);
        let stand_in = RawWindow::fixture(11, 500, built_in(), true);
        assert_eq!(
            fullscreen_gate(&page, std::slice::from_ref(&page), &displays),
            Gate::Clear
        );
        assert_eq!(
            fullscreen_gate(&itself, std::slice::from_ref(&itself), &displays),
            Gate::Itself
        );
        assert_eq!(
            fullscreen_gate(&hidden, &[hidden.clone(), stand_in], &displays),
            Gate::StandIn
        );
    }

    #[test]
    fn an_ax_miss_keeps_the_display_unless_the_window_is_gone() {
        let raw = RawWindow::fixture(10, 500, rect(0.0, 0.0, 100.0, 100.0), false);
        // The Quartz window still exists, on another Space or covered by a stand-in: keep.
        assert_eq!(ax_miss_decision(Ok(&raw)), AxMiss::Keep);
        // The window is gone: release.
        assert_eq!(
            ax_miss_decision(Err(&PlatformError::NotFound)),
            AxMiss::Release
        );
        // Accessibility was revoked: release.
        assert_eq!(
            ax_miss_decision(Err(&PlatformError::PermissionDenied(
                crosspane_platform::Permission::Accessibility
            ))),
            AxMiss::Release
        );
        // A Quartz list that timed out says nothing about the window.
        assert_eq!(ax_miss_decision(Err(&PlatformError::Timeout)), AxMiss::Keep);
        assert_eq!(
            ax_miss_decision(Err(&PlatformError::Backend("window query stopped".into()))),
            AxMiss::Keep
        );
    }

    /// The twin's bounds.
    fn twin_rect() -> RectLogical {
        rect(1800.0, -1406.0, 1710.0, 1406.0)
    }

    /// The parked window under the twin's menu bar.
    fn page(on_screen: bool) -> RawWindow {
        RawWindow::fixture(10, 500, rect(1800.0, -1376.0, 1710.0, 1376.0), on_screen)
    }

    /// WebKit's fullscreen window.
    fn stand_in(frame: RectLogical) -> RawWindow {
        RawWindow::fixture(11, 500, frame, true)
    }

    #[test]
    fn no_ax_write_after_the_window_goes_fullscreen_or_hidden() {
        let twin = &[twin_rect()][..];
        // The pre-check sees a normal window on the twin: a write would be fine.
        let normal = page(true);
        assert_eq!(
            pre_write_decision(&normal, std::slice::from_ref(&normal), twin),
            PreWrite::Write
        );
        // Then, before the write, Safari's fullscreen window appears and the page window leaves
        // the Space. AX would expose only the title-less stand-in: no write, whole display.
        let hidden = page(false);
        let covered = [hidden.clone(), stand_in(twin_rect())];
        assert_eq!(
            pre_write_decision(&hidden, &covered, twin),
            PreWrite::WholeDisplay
        );
        // Or the page window is already off-Space and the stand-in is still animating in (not at
        // the twin's bounds yet): still no write; the retained geometry is reported.
        let animating = [
            hidden.clone(),
            stand_in(rect(1800.0, -1406.0, 1710.0, 700.0)),
        ];
        assert_eq!(
            pre_write_decision(&hidden, &animating, twin),
            PreWrite::Retain
        );
        assert_eq!(
            pre_write_decision(&hidden, std::slice::from_ref(&hidden), twin),
            PreWrite::Retain
        );
        // Or the window itself goes fullscreen (the green button).
        let itself = RawWindow::fixture(10, 500, twin_rect(), true);
        assert_eq!(
            pre_write_decision(&itself, std::slice::from_ref(&itself), twin),
            PreWrite::WholeDisplay
        );
        // Another process's fullscreen window doesn't stop us writing to our showing window.
        let foreign = RawWindow::fixture(12, 600, twin_rect(), true);
        assert_eq!(
            pre_write_decision(&normal, &[normal.clone(), foreign], twin),
            PreWrite::Write
        );
        // No display to compare with: only whether the window is showing counts.
        assert_eq!(
            pre_write_decision(&normal, std::slice::from_ref(&normal), &[]),
            PreWrite::Write
        );
        assert_eq!(pre_write_decision(&hidden, &covered, &[]), PreWrite::Retain);
    }

    #[test]
    fn a_quartz_read_that_fails_never_releases_the_display() {
        let twin = &[twin_rect()][..];
        let read = |raw: RawWindow| Ok((raw.clone(), vec![raw]));
        assert!(matches!(route(read(page(true)), twin), Ok(Plan::Write(_))));
        assert!(matches!(
            route(read(RawWindow::fixture(10, 500, twin_rect(), true)), twin),
            Ok(Plan::WholeDisplay)
        ));
        assert!(matches!(route(read(page(false)), twin), Ok(Plan::Retain)));
        // The pre-check read and the post-mode read both fail (a WindowServer stall during a
        // Space transition), or the query worker stopped: retained geometry, no release.
        assert!(matches!(
            route(Err(PlatformError::Timeout), twin),
            Ok(Plan::Retain)
        ));
        assert!(matches!(
            route(
                Err(PlatformError::Backend("window query stopped".into())),
                twin
            ),
            Ok(Plan::Retain)
        ));
        // Only a window confirmed gone, or a revoked permission, releases it.
        assert!(matches!(
            route(Err(PlatformError::NotFound), twin),
            Err(PlatformError::NotFound)
        ));
        assert!(matches!(
            route(
                Err(PlatformError::PermissionDenied(
                    crosspane_platform::Permission::Accessibility
                )),
                twin
            ),
            Err(PlatformError::PermissionDenied(_))
        ));
    }

    /// A twin state whose Quartz reads come from a script, one reply per read.
    fn scripted_state(replies: Vec<Result<Vec<RawWindow>, PlatformError>>) -> TwinState {
        TwinState {
            journal: PathBuf::new(),
            entries: BTreeMap::from([(
                WindowId(10),
                Entry {
                    pid: 500,
                    frame: rect(0.0, 0.0, 800.0, 600.0),
                    failed_starts: 0,
                },
            )]),
            startup_entries: BTreeSet::from([WindowId(10)]),
            displays: BTreeMap::new(),
            last: BTreeMap::new(),
            last_fullscreen: BTreeMap::new(),
            last_inset: 0.0,
            query: WindowQuery::scripted(replies),
            probe: || Ok(vec![built_in(), twin_rect()]),
            recovery_probe: || Ok(vec![built_in()]),
        }
    }

    #[allow(clippy::unwrap_used)]
    fn journal_state(
        replies: Vec<Result<Vec<RawWindow>, PlatformError>>,
        original: RectLogical,
    ) -> TwinState {
        let mut state = scripted_state(replies);
        state.entries.get_mut(&WindowId(10)).unwrap().frame = original;
        state.journal = std::env::temp_dir().join(format!(
            "crosspane-refused-park-{}-{}.journal",
            std::process::id(),
            crate::clock::now().as_nanos()
        ));
        write_journal(&state.journal, &state.entries).unwrap();
        state
    }

    fn fake_twin(state: &mut TwinState) {
        state.displays.insert(
            WindowId(10),
            VirtualDisplay {
                serial: u32::MAX,
                id: DisplayId(99),
                inset: 0.0,
            },
        );
    }

    fn fake_release(display: VirtualDisplay) -> Result<(), PlatformError> {
        // Numeric fixture only: never run native display release or its Drop cleanup.
        std::mem::forget(display);
        Ok(())
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn parked_fullscreen_ignores_same_pid_fullscreen_on_another_display() {
        let observed = vec![page(true), stand_in(built_in())];
        let state = scripted_state(vec![Ok(observed.clone()), Ok(observed)]);
        let deadline = Instant::now() + FULLSCREEN_WAIT;
        let (_, parked) = state
            .fullscreen_observation(WindowId(10), deadline, &[twin_rect()])
            .unwrap();
        assert_eq!(parked, Gate::Clear);
        // Initial parking and restoration must still inspect every active display.
        assert_eq!(state.gate(WindowId(10)).unwrap().0, Gate::StandIn);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fullscreen_button_rechecks_desired_state_after_lookup_before_press() {
        use std::cell::Cell;
        let normal = page(true);
        let full = RawWindow::fixture(10, 500, twin_rect(), true);
        for desired in [true, false] {
            let (initial, changed) = if desired {
                (normal.clone(), full.clone())
            } else {
                (full.clone(), normal.clone())
            };
            let state = scripted_state(vec![
                Ok(vec![initial]),
                Ok(vec![changed.clone()]),
                Ok(vec![changed.clone()]),
                Ok(vec![changed]),
            ]);
            let deadline = Instant::now() + FULLSCREEN_WAIT;
            let presses = Cell::new(0);
            let result = ensure_fullscreen_with(
                desired,
                || {
                    let (raw, gate) =
                        state.fullscreen_observation(WindowId(10), deadline, &[twin_rect()])?;
                    Ok((raw, gate != Gate::Clear))
                },
                |_| {
                    if state.fullscreen_press_needed(
                        WindowId(10),
                        desired,
                        deadline,
                        &[twin_rect()],
                    )? {
                        presses.set(presses.get() + 1);
                    }
                    Ok(true)
                },
                || Ok(()),
            );
            assert_eq!(presses.get(), 0, "state changed during AX lookup");
            assert!(result.is_ok());
        }
        let state = scripted_state(vec![Ok(vec![page(false)])]);
        assert!(matches!(
            state.fullscreen_press_needed(
                WindowId(10),
                true,
                Instant::now() + FULLSCREEN_WAIT,
                &[twin_rect()]
            ),
            Err(PlatformError::Unsupported(_)),
        ));
    }

    fn fixture_metrics(bounds: RectLogical) -> (DisplayId, RectLogical, PixelSize) {
        (DisplayId(99), bounds, PixelSize::new(1710, 1406))
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fresh_normal_before_ax_miss_and_quartz_timeout_replaces_cached_fullscreen() {
        let window = WindowId(10);
        let normal = page(true);
        let mut state = scripted_state(vec![
            Ok(vec![RawWindow::fixture(10, 500, twin_rect(), true)]),
            Ok(vec![normal.clone()]),
            Err(PlatformError::Timeout),
        ]);
        let metrics = fixture_metrics(twin_rect());
        let initial = state
            .guarded_ax(window, &[twin_rect()], |_, _| {
                panic!("fullscreen has no AX read")
            })
            .unwrap();
        assert!(
            state
                .settled_with_metrics(window, initial, metrics)
                .unwrap()
                .fullscreen
        );
        let observed = state
            .guarded_ax(window, &[twin_rect()], |_, _| Ok(AxStep::Missing))
            .unwrap();
        assert_eq!(observed, Settled::Retained(None));
        let parked = state
            .settled_with_metrics(window, observed, metrics)
            .unwrap();
        assert!(!parked.fullscreen);
        assert_eq!(
            parked.content,
            content(normal.frame, metrics.1, metrics.2).unwrap()
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fresh_fullscreen_before_resize_and_hidden_after_waits_replaces_cached_normal() {
        let window = WindowId(10);
        let normal = page(true);
        let mut state = scripted_state(vec![
            Ok(vec![normal.clone()]),
            Ok(vec![RawWindow::fixture(10, 500, twin_rect(), true)]),
            Ok(vec![page(false)]),
        ]);
        let metrics = fixture_metrics(twin_rect());
        let initial = state
            .guarded_ax(window, &[twin_rect()], |raw, _| {
                Ok(AxStep::Frame(raw.frame))
            })
            .unwrap();
        assert!(
            !state
                .settled_with_metrics(window, initial, metrics)
                .unwrap()
                .fullscreen
        );
        // The real resize path commits this observation before applying/waiting for the mode.
        let before_mode = state.observed_plan(window, &[twin_rect()]).unwrap();
        let fullscreen = matches!(before_mode, Plan::WholeDisplay);
        assert!(fullscreen);
        let requested = Mode::new(PixelSize::new(1200, 900), 1.0).unwrap();
        assert_eq!(
            state
                .resize_mode_for_plan(window, requested, fullscreen)
                .unwrap()
                .0,
            requested
        );
        let final_bounds = rect(1800.0, -900.0, 1200.0, 900.0);
        let final_metrics = fixture_metrics(final_bounds);
        let parked = state
            .resized_fullscreen_with_metrics(window, final_metrics)
            .unwrap();
        assert!(parked.fullscreen);
        assert_eq!(
            parked.content,
            content(final_bounds, final_bounds, final_metrics.2).unwrap()
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fullscreen_exit_then_ax_miss_uses_fresh_normal_frame_and_clears_cached_bit() {
        let fresh = RawWindow::fixture(10, 500, rect(1850.0, -1300.0, 800.0, 600.0), true);
        let mut state = scripted_state(vec![Ok(vec![fresh.clone()]), Ok(vec![fresh.clone()])]);
        let window = WindowId(10);
        let metrics = fixture_metrics(twin_rect());
        state.last.insert(window, page(true).frame);
        assert!(
            state
                .settled_with_metrics(window, Settled::WholeDisplay, metrics)
                .unwrap()
                .fullscreen
        );
        let settled = state
            .guarded_ax(window, &[twin_rect()], |_, _| Ok(AxStep::Missing))
            .unwrap();
        assert_eq!(settled, Settled::Retained(Some(fresh.frame)));
        let parked = state
            .settled_with_metrics(window, settled, metrics)
            .unwrap();
        assert!(!parked.fullscreen);
        assert_eq!(
            parked.content,
            content(fresh.frame, metrics.1, metrics.2).unwrap()
        );
        assert_eq!(state.last.get(&window), Some(&fresh.frame));
        assert_eq!(state.last_fullscreen.get(&window), Some(&false));
        let hidden = state
            .settled_with_metrics(window, Settled::Retained(None), metrics)
            .unwrap();
        assert_eq!(hidden, parked);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fullscreen_resize_reobserves_normal_hidden_closed_and_fullscreen_after_waits() {
        let final_bounds = rect(1800.0, -900.0, 1200.0, 900.0);
        let normal = RawWindow::fixture(10, 500, rect(1820.0, -850.0, 800.0, 600.0), true);
        let metrics = fixture_metrics(final_bounds);
        for (after_waits, expected) in [
            (Ok(vec![normal.clone()]), Some(false)),
            (Ok(vec![page(false)]), Some(true)),
            (Ok(vec![]), None),
            (Err(PlatformError::Timeout), Some(true)),
            (
                Ok(vec![RawWindow::fixture(10, 500, final_bounds, true)]),
                Some(true),
            ),
            (Ok(vec![page(false), stand_in(final_bounds)]), Some(true)),
        ] {
            let mut state = scripted_state(vec![
                Ok(vec![RawWindow::fixture(10, 500, twin_rect(), true)]),
                after_waits,
            ]);
            let window = WindowId(10);
            assert!(matches!(
                state.plan(window, &[twin_rect()]).unwrap(),
                Plan::WholeDisplay
            ));
            state
                .settled_with_metrics(window, Settled::WholeDisplay, fixture_metrics(twin_rect()))
                .unwrap();
            let result = state.resized_fullscreen_with_metrics(window, metrics);
            match expected {
                Some(fullscreen) => {
                    let parked = result.unwrap();
                    assert_eq!(parked.fullscreen, fullscreen);
                    if !fullscreen {
                        assert_eq!(
                            parked.content,
                            content(normal.frame, metrics.1, metrics.2).unwrap()
                        );
                    }
                }
                None => assert!(matches!(result, Err(PlatformError::NotFound))),
            }
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fullscreen_resize_at_capacity_ignores_stale_menu_inset() {
        for scale in [1.0, 2.0] {
            let mut state = scripted_state(vec![]);
            fake_twin(&mut state);
            state.displays.get_mut(&WindowId(10)).unwrap().inset = 30.0;
            let requested = Mode::new(PixelSize::new(8192, 8192), scale).unwrap();
            let full = state.resize_mode_for_plan(WindowId(10), requested, true);
            let normal = state.resize_mode_for_plan(WindowId(10), requested, false);
            // Dispose of the numeric fixture before assertions, including a red failure.
            fake_release(state.displays.remove(&WindowId(10)).unwrap()).unwrap();
            assert_eq!(full.unwrap(), (requested, 0.0));
            assert!(matches!(normal, Err(PlatformError::Unsupported(_))));
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn abort_keeps_owned_twin_and_journal_on_ax_miss_with_existing_quartz_window() {
        for error in [
            PlatformError::NotFound,
            PlatformError::Backend("AX missing".into()),
        ] {
            let mut state = journal_state(vec![Ok(vec![page(true)])], built_in());
            fake_twin(&mut state);
            let journal = fs::read(&state.journal).unwrap();
            let before = state.entries.clone();
            let expected = error.to_string();
            let actual = state.abort_with(WindowId(10), error, |_, _| {
                panic!("an AX miss must not restore or release this window")
            });
            let retained = state.displays.contains_key(&WindowId(10));
            // Remove the numeric fixture without invoking native display release or Drop.
            for (_, display) in std::mem::take(&mut state.displays) {
                fake_release(display).unwrap();
            }
            assert_eq!(actual.to_string(), expected);
            assert!(retained);
            assert_eq!(state.entries, before);
            assert_eq!(fs::read(&state.journal).unwrap(), journal);
            fs::remove_file(&state.journal).unwrap();
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fullscreen_pre_write_refusal_forgets_new_entry_and_preserves_original_error() {
        for cleanup_fails in [false, true] {
            let window = WindowId(10);
            let original = built_in();
            let mut state = journal_state(
                vec![Ok(vec![RawWindow::fixture(10, 500, original, true)])],
                original,
            );
            let released = std::cell::Cell::new(0);
            let message = "window is fullscreen or not showing; not parked";
            let error = state
                .park_sequence_with(
                    window,
                    ParkAttempt::new(true, original, original),
                    |state| {
                        fake_twin(state);
                        Ok((original, twin_rect()))
                    },
                    (
                        |_| -> Result<(), PlatformError> {
                            panic!("fullscreen plan must refuse before AX lookup")
                        },
                        |(), _, _| panic!("pre-write refusal must not write AX"),
                    ),
                    |_| panic!("refused park has no geometry"),
                    (
                        |display| {
                            released.set(released.get() + 1);
                            fake_release(display)?;
                            if cleanup_fails {
                                Err(PlatformError::Backend("fake release failed".into()))
                            } else {
                                Ok(())
                            }
                        },
                        |_, _, _| panic!("pre-write refusal must not attempt restoration"),
                    ),
                )
                .unwrap_err();
            assert!(matches!(error, PlatformError::Backend(text) if text == message));
            assert_eq!(released.get(), 1);
            assert!(state.displays.is_empty());
            assert!(state.entries.is_empty());
            assert!(read_journal(&state.journal).unwrap().is_empty());
            fs::remove_file(&state.journal).unwrap();
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn partial_ax_size_write_then_position_guard_refusal_rolls_back_and_retains_entry() {
        let window = WindowId(10);
        let original = built_in();
        let target = rect(1800.0, -600.0, 500.0, 400.0);
        // Fresh plan, AXSize guard, AXPosition guard: fullscreen begins after AXSize succeeds.
        let mut state = journal_state(
            vec![
                Ok(vec![RawWindow::fixture(10, 500, original, true)]),
                Ok(vec![RawWindow::fixture(10, 500, original, true)]),
                Ok(vec![RawWindow::fixture(10, 500, twin_rect(), true)]),
            ],
            original,
        );
        let journal = fs::read(&state.journal).unwrap();
        let actual = std::cell::Cell::new(original);
        let size_writes = std::cell::Cell::new(0);
        let position_writes = std::cell::Cell::new(0);
        let rollback = std::cell::Cell::new(0);
        let restore_writes = std::cell::Cell::new(0);
        let message = "window went fullscreen or left the screen; not parked";
        let error = state
            .park_sequence_with(
                window,
                ParkAttempt::new(true, original, original),
                |state| {
                    fake_twin(state);
                    Ok((twin_rect(), target))
                },
                (
                    |raw| {
                        assert_eq!(raw.frame, original);
                        Ok(())
                    },
                    |(), target, allowed| {
                        // Fake restore_guarded follows AxWindow's size/guard/position ordering.
                        assert!(allowed());
                        size_writes.set(size_writes.get() + 1);
                        actual.set(RectLogical::new(original.origin, target.size));
                        if !allowed() {
                            return Err(PlatformError::Backend(message.into()));
                        }
                        position_writes.set(position_writes.get() + 1);
                        actual.set(target);
                        Ok(target)
                    },
                ),
                |_| panic!("partial write never reaches parked geometry"),
                (
                    |display| {
                        fake_release(display)?;
                        panic!("after a write the existing rollback owns display release")
                    },
                    |state, window, error| {
                        rollback.set(rollback.get() + 1);
                        let restored = state.restore_with(
                            window,
                            |_, _| {
                                restore_writes.set(restore_writes.get() + 1);
                                Err(PlatformError::Backend("fake restore failed".into()))
                            },
                            fake_release,
                        );
                        assert!(
                            matches!(restored, Err(PlatformError::Backend(text)) if text == "fake restore failed")
                        );
                        error
                    },
                ),
            )
            .unwrap_err();
        assert!(matches!(error, PlatformError::Backend(text) if text == message));
        assert_eq!(size_writes.get(), 1);
        assert_eq!(position_writes.get(), 0);
        assert_eq!(actual.get(), RectLogical::new(original.origin, target.size));
        assert_eq!(rollback.get(), 1);
        assert_eq!(restore_writes.get(), 1);
        assert!(state.displays.is_empty());
        assert_eq!(state.entries.get(&window).unwrap().frame, original);
        assert_eq!(fs::read(&state.journal).unwrap(), journal);
        fs::remove_file(&state.journal).unwrap();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn refused_repark_preserves_unmatched_prior_journal_entry() {
        let window = WindowId(10);
        let original = rect(20.0, 30.0, 800.0, 600.0);
        let current = built_in();
        let mut state = journal_state(
            vec![Ok(vec![RawWindow::fixture(10, 500, current, true)])],
            original,
        );
        let journal = fs::read(&state.journal).unwrap();
        let error = state
            .park_sequence_with(
                window,
                ParkAttempt::new(false, current, original),
                |state| {
                    fake_twin(state);
                    Ok((current, twin_rect()))
                },
                (
                    |_| -> Result<(), PlatformError> {
                        panic!("pre-write refusal must not lookup AX")
                    },
                    |(), _, _| panic!("pre-write refusal must not write AX"),
                ),
                |_| panic!("refused re-park has no geometry"),
                (fake_release, |_, _, _| {
                    panic!("pre-write re-park must not restore a prior window")
                }),
            )
            .unwrap_err();
        assert!(
            matches!(error, PlatformError::Backend(text) if text == "window is fullscreen or not showing; not parked")
        );
        assert!(state.displays.is_empty());
        assert_eq!(state.entries.get(&window).unwrap().frame, original);
        assert_eq!(fs::read(&state.journal).unwrap(), journal);
        fs::remove_file(&state.journal).unwrap();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn refused_repark_forgets_prior_entry_proven_home() {
        let window = WindowId(10);
        let original = built_in();
        let mut state = journal_state(vec![], original);
        fake_twin(&mut state);
        let error = state
            .park_sequence_with(
                window,
                ParkAttempt::new(false, original, original),
                |_| Err(PlatformError::Timeout),
                (
                    |_| -> Result<(), PlatformError> { panic!("setup failed before AX lookup") },
                    |(), _, _| panic!("setup failed before AX write"),
                ),
                |_| panic!("setup failed before geometry"),
                (fake_release, |_, _, _| {
                    panic!("a proven-home entry needs no restoration")
                }),
            )
            .unwrap_err();
        assert!(matches!(error, PlatformError::Timeout));
        assert!(state.displays.is_empty());
        assert!(state.entries.is_empty());
        assert!(read_journal(&state.journal).unwrap().is_empty());
        fs::remove_file(&state.journal).unwrap();
    }

    struct RecoveryFixture(TwinState);

    impl RecoveryFixture {
        fn new(replies: Vec<Result<Vec<RawWindow>, PlatformError>>, original: RectLogical) -> Self {
            Self(journal_state(replies, original))
        }
    }

    impl Drop for RecoveryFixture {
        fn drop(&mut self) {
            // Only the scratch journal this fixture created; no native displays are constructed.
            let _ = fs::remove_file(&self.0.journal);
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn recovery_refused_frame_on_real_display_removes_entry() {
        let window = WindowId(10);
        let original = rect(20.0, 30.0, 800.0, 600.0);
        let clamped = RawWindow::fixture(10, 500, rect(20.0, 30.0, 800.0, 500.0), true);
        for after_attempt in [false, true] {
            let mut replies = Vec::new();
            if after_attempt {
                replies.push(Ok(vec![page(true)]));
            }
            replies.push(Ok(vec![clamped.clone()]));
            let mut fixture = RecoveryFixture::new(replies, original);
            let mut attempts = 0;
            let restored = fixture
                .0
                .recover_with(
                    |_, _| {
                        attempts += 1;
                        Err(PlatformError::Backend(
                            "window refused original frame; journal retained".into(),
                        ))
                    },
                    TwinState::window_on_real_display,
                )
                .unwrap();
            assert_eq!(restored, vec![window]);
            assert_eq!(attempts, u32::from(after_attempt));
            assert!(fixture.0.entries.is_empty());
            assert!(read_journal(&fixture.0.journal).unwrap().is_empty());
        }
        // A confirmed absence needs no display or AX read, and is not counted as restoration.
        let mut fixture = RecoveryFixture::new(vec![Ok(vec![])], original);
        fixture.0.recovery_probe = || panic!("absent window requires no display query");
        let restored = fixture
            .0
            .recover_with(
                |_, _| panic!("absent window requires no AX restore"),
                TwinState::window_on_real_display,
            )
            .unwrap();
        assert!(restored.is_empty());
        assert!(read_journal(&fixture.0.journal).unwrap().is_empty());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn recovery_ax_cannot_complete_on_real_display_removes_entry() {
        let window = WindowId(10);
        // The unrestricted Quartz query retains even a helper excluded from Browse listings.
        let helper = RawWindow::fixture(10, 500, rect(40.0, 50.0, 1.0, 1.0), true);
        for after_attempt in [false, true] {
            let mut replies = Vec::new();
            if after_attempt {
                replies.push(Ok(vec![page(true)]));
            }
            replies.push(Ok(vec![helper.clone()]));
            let mut fixture = RecoveryFixture::new(replies, rect(20.0, 30.0, 800.0, 600.0));
            let mut attempts = 0;
            let restored = fixture
                .0
                .recover_with(
                    |_, _| {
                        attempts += 1;
                        Err(PlatformError::Backend(
                            "AX error -25200 (kAXErrorCannotComplete)".into(),
                        ))
                    },
                    TwinState::window_on_real_display,
                )
                .unwrap();
            assert_eq!(restored, vec![window]);
            assert_eq!(attempts, u32::from(after_attempt));
            assert!(fixture.0.entries.is_empty());
            assert!(read_journal(&fixture.0.journal).unwrap().is_empty());
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn recovery_failing_twin_keeps_incremented_count_once_per_start() {
        let window = WindowId(10);
        let original = rect(20.0, 30.0, 800.0, 600.0);
        let mut fixture =
            RecoveryFixture::new(vec![Ok(vec![page(true)]), Ok(vec![page(true)])], original);
        let fail =
            |_: &mut TwinState, _: WindowId| Err(PlatformError::Backend("AX error -25200".into()));
        assert!(
            fixture
                .0
                .recover_with(fail, TwinState::window_on_real_display)
                .is_err()
        );
        assert_eq!(fixture.0.entries[&window].failed_starts, 1);
        assert_eq!(
            read_journal(&fixture.0.journal).unwrap()[&window].failed_starts,
            1
        );
        let journal = fs::read(&fixture.0.journal).unwrap();
        // Same-process recovery (including shutdown) must use the old restore path, not count
        // another failed start or relax its real-display/AX requirements.
        assert!(
            fixture
                .0
                .recover_with(fail, |_, _| panic!("startup observer reused at shutdown"))
                .is_err()
        );
        assert_eq!(fixture.0.entries[&window].failed_starts, 1);
        assert_eq!(fs::read(&fixture.0.journal).unwrap(), journal);
        // Real-display proof refuses hidden, invalid, outside and only partly contained frames.
        let displays = [built_in()];
        let mut hidden = RawWindow::fixture(10, 500, original, false);
        assert!(!on_real_display(&hidden, &displays));
        hidden.on_screen = true;
        for frame in [
            rect(0.0, 0.0, 0.0, 1.0),
            twin_rect(),
            rect(-1.0, 0.0, 10.0, 10.0),
        ] {
            hidden.frame = frame;
            assert!(!on_real_display(&hidden, &displays));
        }
        // NotFound from the display query is not proof that a present window vanished.
        let mut unknown = RecoveryFixture::new(
            (0..2)
                .map(|_| Ok(vec![RawWindow::fixture(10, 500, original, true)]))
                .collect(),
            original,
        );
        unknown.0.recovery_probe = || Err(PlatformError::NotFound);
        assert!(
            unknown
                .0
                .recover_with(fail, TwinState::window_on_real_display)
                .is_err()
        );
        assert_eq!(unknown.0.entries[&window].failed_starts, 1);
        assert_eq!(
            read_journal(&unknown.0.journal).unwrap()[&window].failed_starts,
            1
        );
        // A reused numeric window id with a foreign PID is absence of the journaled window.
        let foreign = RawWindow::fixture(10, 501, original, true);
        assert!(matches!(
            fixture.0.pick(window, vec![foreign]),
            Err(PlatformError::NotFound)
        ));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn recovery_third_failed_start_drops_entry_without_claiming_restored() {
        let window = WindowId(10);
        let mut fixture =
            RecoveryFixture::new(vec![Ok(vec![page(true)]), Ok(vec![page(true)])], built_in());
        fixture.0.entries.get_mut(&window).unwrap().failed_starts = 2;
        write_journal(&fixture.0.journal, &fixture.0.entries).unwrap();
        assert_eq!(
            read_journal(&fixture.0.journal).unwrap()[&window].failed_starts,
            2
        );
        let restored = fixture
            .0
            .recover_with(
                |_, _| Err(PlatformError::Backend("AX error -25200".into())),
                TwinState::window_on_real_display,
            )
            .unwrap();
        assert!(restored.is_empty());
        assert!(fixture.0.entries.is_empty());
        assert!(read_journal(&fixture.0.journal).unwrap().is_empty());
        assert!(
            fixture
                .0
                .recover_with(
                    |_, _| panic!("retired entry retried"),
                    |_, _| panic!("retired entry observed")
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn recovery_old_journal_line_defaults_failed_start_count_to_zero() {
        let window = WindowId(42);
        let old = "crosspane-twin-v1\n42 123 -100.5 40.25 400 300\n";
        let mut entries = parse_journal(old).unwrap();
        assert_eq!(entries[&window].failed_starts, 0);
        entries.get_mut(&window).unwrap().failed_starts = 2;
        let new = format!(
            "crosspane-twin-v1\n{}\n",
            journal_line(window, &entries[&window])
        );
        assert_eq!(parse_journal(&new).unwrap(), entries);
        for count in ["-1", "256", "bad", "1 extra"] {
            assert!(
                parse_journal(&format!(
                    "crosspane-twin-v1\n42 123 -100.5 40.25 400 300 {count}\n"
                ))
                .is_err()
            );
        }
        // Mixed old/new lines remain readable, while all existing strict frame checks remain.
        assert_eq!(
            parse_journal(&format!("{old}43 124 0 0 400 300 1\n"))
                .unwrap()
                .len(),
            2
        );
        assert!(parse_journal("crosspane-twin-v1\n42 123 NaN 0 400 300 1\n").is_err());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn recover_at_original_frame_removes_fullscreen_and_ordinary_entries_without_ax() {
        let window = WindowId(10);
        for (original, expected_gate) in [
            (built_in(), Gate::Itself),
            (rect(20.0, 30.0, 800.0, 600.0), Gate::Clear),
        ] {
            let raw = RawWindow::fixture(10, 500, original, true);
            assert_eq!(
                fullscreen_gate(&raw, std::slice::from_ref(&raw), &[built_in()]),
                expected_gate
            );
            let mut state = journal_state(vec![Ok(vec![raw])], original);
            state.probe = || panic!("already-home recovery must finish before gates or AX");
            let path = state.journal.clone();
            // Construct with injected Quartz data; never preflight private APIs or owner resources.
            let mut parking = MacTwinParking {
                state: Mutex::new(state),
            };
            assert_eq!(parking.recover().unwrap(), vec![window]);
            assert!(parking.state().unwrap().entries.is_empty());
            assert!(parking.state().unwrap().displays.is_empty());
            assert!(read_journal(&path).unwrap().is_empty());
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn recover_elsewhere_still_keeps_journal_when_restore_cannot_proceed() {
        let window = WindowId(10);
        let original = rect(20.0, 30.0, 800.0, 600.0);
        let mut state = journal_state(
            (0..3)
                .map(|_| Ok(vec![RawWindow::fixture(10, 500, built_in(), true)]))
                .collect(),
            original,
        );
        state.probe = || Err(PlatformError::Backend("fake display lookup failed".into()));
        state.recovery_probe = state.probe;
        let path = state.journal.clone();
        let mut parking = MacTwinParking {
            state: Mutex::new(state),
        };
        assert!(
            matches!(parking.recover(), Err(PlatformError::Backend(text)) if text == "fake display lookup failed")
        );
        assert_eq!(
            parking.state().unwrap().entries.get(&window).unwrap().frame,
            original
        );
        assert_eq!(read_journal(&path).unwrap()[&window].failed_starts, 1);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn every_call_site_routes_its_quartz_read_the_same_way() {
        let twin = &[twin_rect()][..];
        let id = WindowId(10);
        // resize: the pre-mode check fails, the look before the write sees a stand-in; then
        // geometry's read fails; then the window is gone from the list.
        let state = scripted_state(vec![
            Err(PlatformError::Timeout),
            Ok(vec![page(false), stand_in(twin_rect())]),
            Err(PlatformError::Backend("window query stopped".into())),
            Ok(vec![stand_in(twin_rect())]),
        ]);
        assert!(matches!(state.plan(id, twin), Ok(Plan::Retain)));
        assert!(matches!(state.plan(id, twin), Ok(Plan::WholeDisplay)));
        assert!(matches!(state.plan(id, twin), Ok(Plan::Retain)));
        assert!(matches!(state.plan(id, twin), Err(PlatformError::NotFound)));
        // The transition sequence through the real read path: normal at the pre-check, then the
        // write is refused whether the page window is covered or merely hidden.
        let state = scripted_state(vec![
            Ok(vec![page(true)]),
            Ok(vec![page(false), stand_in(twin_rect())]),
        ]);
        assert!(matches!(state.plan(id, twin), Ok(Plan::Write(_))));
        assert!(matches!(state.plan(id, twin), Ok(Plan::WholeDisplay)));
        let state = scripted_state(vec![Ok(vec![page(true)]), Ok(vec![page(false)])]);
        assert!(matches!(state.plan(id, twin), Ok(Plan::Write(_))));
        assert!(matches!(state.plan(id, twin), Ok(Plan::Retain)));
    }

    /// Run one guarded AX step against a script: the closure stands in for the AX lookup and
    /// frame writes, and asks the guard (a scripted Quartz read) the way `ax_move` does. Returns
    /// the settled result and how many writes the closure made.
    fn run_step(
        replies: Vec<Result<Vec<RawWindow>, PlatformError>>,
        step: impl FnOnce(&mut dyn FnMut() -> bool, &dyn Fn()) -> Result<AxStep, PlatformError>,
    ) -> (Result<Settled, PlatformError>, u32) {
        let mut state = scripted_state(replies);
        let writes = std::cell::Cell::new(0);
        let note_write = || writes.set(writes.get() + 1);
        let settled = state.guarded_ax(WindowId(10), &[twin_rect()], |_raw, guard| {
            step(guard, &note_write)
        });
        (settled, writes.get())
    }

    fn took() -> Result<AxStep, PlatformError> {
        Ok(AxStep::Frame(rect(1800.0, -1376.0, 1710.0, 1376.0)))
    }

    fn refused() -> PlatformError {
        PlatformError::Backend("AX refused the write".into())
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_write_refused_because_the_window_went_fullscreen_is_not_an_abort() {
        let covered = || Ok(vec![page(false), stand_in(twin_rect())]);
        // The window goes fullscreen after the plan and before the write: the guard, a fresh read
        // after the AX lookup, says no. Nothing is written, and the re-read settles on the whole
        // display.
        let (settled, writes) = run_step(
            vec![Ok(vec![page(true)]), covered(), covered()],
            |guard, write| {
                if !guard() {
                    return Ok(AxStep::Withheld);
                }
                write();
                took()
            },
        );
        assert_eq!(settled.unwrap(), Settled::WholeDisplay);
        assert_eq!(writes, 0);
        // The window goes fullscreen right after the guard said yes: AX takes or refuses the
        // write on the fullscreen window. The failure doesn't abort: the re-read says whole
        // display.
        let (settled, writes) = run_step(
            vec![Ok(vec![page(true)]), Ok(vec![page(true)]), covered()],
            |guard, write| {
                assert!(guard());
                write();
                Err(refused())
            },
        );
        assert_eq!(settled.unwrap(), Settled::WholeDisplay);
        assert_eq!(writes, 1);
        // The same when the window itself is the fullscreen one.
        let itself = || Ok(vec![RawWindow::fixture(10, 500, twin_rect(), true)]);
        let (settled, _) = run_step(
            vec![Ok(vec![page(true)]), Ok(vec![page(true)]), itself()],
            |guard, _| {
                assert!(guard());
                Err(refused())
            },
        );
        assert_eq!(settled.unwrap(), Settled::WholeDisplay);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_failed_step_aborts_only_when_the_window_is_still_showing_or_gone() {
        // Hidden after the failure, or the re-read is inconclusive: retained geometry.
        let (settled, _) = run_step(
            vec![
                Ok(vec![page(true)]),
                Ok(vec![page(true)]),
                Ok(vec![page(false)]),
            ],
            |guard, _| {
                assert!(guard());
                Err(refused())
            },
        );
        assert_eq!(settled.unwrap(), Settled::Retained(None));
        let (settled, _) = run_step(
            vec![
                Ok(vec![page(true)]),
                Ok(vec![page(true)]),
                Err(PlatformError::Timeout),
            ],
            |guard, _| {
                assert!(guard());
                Err(refused())
            },
        );
        assert_eq!(settled.unwrap(), Settled::Retained(None));
        // Still showing normally: the existing error path.
        let (settled, _) = run_step(
            vec![
                Ok(vec![page(true)]),
                Ok(vec![page(true)]),
                Ok(vec![page(true)]),
            ],
            |guard, _| {
                assert!(guard());
                Err(refused())
            },
        );
        assert!(matches!(settled, Err(PlatformError::Backend(_))));
        // Confirmed gone after the failure: release.
        let (settled, _) = run_step(
            vec![Ok(vec![page(true)]), Ok(vec![page(true)]), Ok(vec![])],
            |guard, _| {
                assert!(guard());
                Err(refused())
            },
        );
        assert!(matches!(settled, Err(PlatformError::NotFound)));
        // A revoked permission releases at once, without another read.
        let (settled, _) = run_step(vec![Ok(vec![page(true)])], |_, _| {
            Err(PlatformError::PermissionDenied(
                crosspane_platform::Permission::Accessibility,
            ))
        });
        assert!(matches!(settled, Err(PlatformError::PermissionDenied(_))));
        // A step that took needs no re-read (the script has none left: a read would time out).
        let (settled, writes) = run_step(vec![Ok(vec![page(true)])], |_, write| {
            write();
            took()
        });
        assert!(matches!(settled, Ok(Settled::Frame(_))));
        assert_eq!(writes, 1);
        // A miss with the window still showing keeps it at its frame; a hidden one at no frame.
        let (settled, _) = run_step(vec![Ok(vec![page(true)]), Ok(vec![page(true)])], |_, _| {
            Ok(AxStep::Missing)
        });
        assert_eq!(
            settled.unwrap(),
            Settled::Retained(Some(rect(1800.0, -1376.0, 1710.0, 1376.0)))
        );
        // A window that was not writable at the plan is settled without running the step at all.
        let (settled, writes) = run_step(vec![Ok(vec![page(false)])], |_, write| {
            write();
            took()
        });
        assert_eq!(settled.unwrap(), Settled::Retained(None));
        assert_eq!(writes, 0);
    }

    #[test]
    fn restore_and_park_see_a_fullscreen_window_without_owning_a_twin() {
        let id = WindowId(10);
        let on_built_in = || Ok(vec![RawWindow::fixture(10, 500, built_in(), true)]);
        // After a release or a restart: no twin is owned, and the window fills the built-in
        // display (macOS moved its fullscreen Space there). It is the window itself: its button
        // can take it out, `recover` waits for that and re-reads before writing.
        let state = scripted_state(vec![on_built_in()]);
        assert!(state.displays.is_empty());
        assert!(matches!(state.gate(id), Ok((Gate::Itself, _))));
        // A stand-in fills it and the page window is off-Space: no write, journal kept.
        let state = scripted_state(vec![Ok(vec![
            page(false),
            RawWindow::fixture(11, 500, built_in(), true),
        ])]);
        assert!(matches!(state.gate(id), Ok((Gate::StandIn, _))));
        // The initial park: a clear window goes ahead, and an inconclusive read neither parks
        // nor releases anything (no twin exists).
        let state = scripted_state(vec![Ok(vec![page(true)]), Err(PlatformError::Timeout)]);
        assert!(matches!(state.gate(id), Ok((Gate::Clear, _))));
        assert!(matches!(state.gate(id), Err(PlatformError::Timeout)));
        // The window is gone: `restore` has nothing to do.
        let state = scripted_state(vec![Ok(vec![])]);
        assert!(matches!(state.gate(id), Err(PlatformError::NotFound)));
        // The displays can't be listed: the gate fails closed.
        let mut state = scripted_state(vec![Ok(vec![page(true)])]);
        state.probe = || Err(PlatformError::Backend("no display list".into()));
        assert!(state.gate(id).is_err());
        assert!(!state.write_allowed(id));
        // After the window left fullscreen, a write is allowed again.
        let state = scripted_state(vec![Ok(vec![page(true)])]);
        assert!(state.write_allowed(id));
    }

    #[test]
    fn polling_sleeps_are_clamped_to_the_remaining_budget() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(2);
        let poll = Duration::from_millis(50);
        assert_eq!(next_sleep(deadline, now, poll), Some(poll));
        let near = deadline - Duration::from_millis(20);
        assert_eq!(
            next_sleep(deadline, near, poll),
            Some(Duration::from_millis(20))
        );
        assert_eq!(next_sleep(deadline, deadline, poll), None);
        assert_eq!(
            next_sleep(deadline, deadline + Duration::from_millis(1), poll),
            None
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn kept_content_is_the_last_frame_clipped_to_the_twin_or_the_whole_twin() {
        let bounds = rect(1800.0, -600.0, 800.0, 600.0);
        let pixels = PixelSize::new(1600, 1200);
        let whole = PixelRect::new(euclid::Point2D::new(0, 0), euclid::Point2D::new(1600, 1200));
        // Where the window last was, in the twin's current pixels.
        assert_eq!(
            kept_content(Some(rect(1830.0, -580.0, 200.0, 100.0)), bounds, pixels).unwrap(),
            PixelRect::new(euclid::Point2D::new(60, 40), euclid::Point2D::new(460, 240))
        );
        // The twin shrank under the window: clipped to what is left.
        assert_eq!(
            kept_content(Some(rect(1790.0, -610.0, 900.0, 700.0)), bounds, pixels).unwrap(),
            whole
        );
        // Never framed, or the twin moved away from the old frame: the whole twin.
        assert_eq!(kept_content(None, bounds, pixels).unwrap(), whole);
        assert_eq!(
            kept_content(Some(rect(0.0, 0.0, 100.0, 100.0)), bounds, pixels).unwrap(),
            whole
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn journal_round_trip() {
        let directory = std::env::temp_dir().join(format!(
            "crosspane-twin-{}-{}",
            std::process::id(),
            crate::clock::now().as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("journal");
        assert!(read_journal(&path).unwrap().is_empty());
        let entries = BTreeMap::from([(
            WindowId(42),
            Entry {
                pid: 123,
                frame: RectLogical::new(
                    PointLogical::new(-100.5, 40.25),
                    SizeLogical::new(400.0, 300.0),
                ),
                failed_starts: 0,
            },
        )]);
        write_journal(&path, &entries).unwrap();
        assert_eq!(read_journal(&path).unwrap(), entries);
        write_journal(&path, &BTreeMap::new()).unwrap();
        assert!(read_journal(&path).unwrap().is_empty());
        fs::write(&path, "crosspane-twin-v1\n42 123 NaN 0 400 300\n").unwrap();
        assert!(read_journal(&path).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
    pub(crate) fn gui_lifecycle() {
        // Read-only probe: the event is created but never posted.
        let event = objc2_core_graphics::CGEvent::new(None).expect("unposted clock probe");
        let timestamp = objc2_core_graphics::CGEvent::timestamp(Some(&event));
        // Public declaration from Apple's <mach/mach_time.h>.
        unsafe extern "C" {
            fn mach_absolute_time() -> u64;
        }
        // SAFETY: reads the public monotonic counter, with no arguments and no side effects.
        let ticks = unsafe { mach_absolute_time() };
        let now = crate::clock::now().as_nanos();
        println!("timestamp probe: CGEvent={timestamp} mach_ticks={ticks} clock_now_ns={now}");
        if timestamp == 0 {
            eprintln!(
                "timestamp probe inconclusive: unposted CGEvent has timestamp zero; twin parking has no at fields"
            );
        } else {
            println!(
                "timestamp differences: ticks={}, ns={}",
                timestamp.abs_diff(ticks),
                timestamp.abs_diff(now)
            );
        }
        let classes = classes().expect("constructor API preflight");
        for (class, selector, expected) in api_methods(classes) {
            println!(
                "ABI: {:?} {:?} {:?}",
                class.name(),
                selector.name(),
                method_encoding(class.instance_method(selector).unwrap()).unwrap()
            );
            assert!(encoding_matches(
                method_encoding(class.instance_method(selector).unwrap()),
                expected
            ));
        }
        let initial = Mode::new(PixelSize::new(1600, 1200), 2.0).unwrap();
        let display = VirtualDisplay::create(initial).unwrap();
        let id = display.id;
        // All assertions happen while the RAII display handle is alive, so unwind releases it.
        wait_mode(id, initial).unwrap();
        println!(
            "created twin {:?}: 1600x1200 pixels, bounds={:?}",
            id,
            CGDisplayBounds(id.0)
        );
        let larger = Mode::new(PixelSize::new(MAX_PIXELS, MAX_PIXELS), 2.0).unwrap();
        display.apply(larger).unwrap();
        if let Err(error) = wait_mode(id, larger) {
            display.release().unwrap();
            println!("twin {:?} released and absent after failed growth", id);
            panic!("larger mode did not settle: {error}");
        }
        println!(
            "grew twin {:?}: {}x{} pixels, descriptor maximum={}x{}, bounds={:?}",
            id,
            larger.pixels.width,
            larger.pixels.height,
            MAX_PIXELS,
            MAX_PIXELS,
            CGDisplayBounds(id.0)
        );
        assert!(matches!(
            Mode::new(PixelSize::new(MAX_PIXELS + 1, MAX_PIXELS), 2.0),
            Err(PlatformError::Unsupported(_))
        ));
        wait_mode(id, larger).unwrap();
        let placement = place_twin(id);
        if let Ok(placement) = placement.as_ref() {
            println!(
                "placed twin {:?}: attempt={}, origin=({}, {}), {}",
                id,
                placement.attempt,
                placement.frame.origin.x,
                placement.frame.origin.y,
                if placement.corner_only {
                    "corner-only"
                } else {
                    "edge-adjacent; pointer can reach it (v0 limitation)"
                }
            );
        }
        display.release().unwrap();
        wait_display(id, false).unwrap();
        println!("twin {:?} released and absent", id);
        placement.unwrap();
        println!("private_vdisplay_gui: 1 passed");
    }

    #[allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
    pub(crate) fn live_textedit() {
        // The lead supplies a disposable window. Never launch, activate, inject, or capture.
        let windows = crate::windows::MacWindows::new().unwrap();
        let window = crosspane_platform::WindowSource::windows(&windows)
            .unwrap()
            .into_iter()
            .find(|window| window.app_id == "com.apple.TextEdit")
            .expect("lead must open disposable TextEdit window");
        let path = std::env::temp_dir().join(format!(
            "crosspane-twin-textedit-{}-{}.journal",
            std::process::id(),
            window.id.0
        ));
        let mut parking = MacTwinParking::new(path.clone()).unwrap();
        let real: Vec<_> = active_displays()
            .unwrap()
            .into_iter()
            .map(|id| CGDisplayBounds(id))
            .collect();
        let parked = parking
            .park(window.id, PixelSize::new(1600, 1200), 2.0)
            .unwrap();
        assert_eq!(parked.kind, ParkingKind::Twin);
        assert!(active_displays().unwrap().contains(&parked.display.0));
        assert_eq!(parking.geometry(window.id).unwrap(), parked);
        let actual = parking.state().unwrap().quartz(window.id).unwrap().0.frame;
        let twin = CGDisplayBounds(parked.display.0);
        assert!(
            actual.min_x() >= twin.origin.x && actual.max_x() <= twin.origin.x + twin.size.width
        );
        assert!(
            actual.min_y() >= twin.origin.y && actual.max_y() <= twin.origin.y + twin.size.height
        );
        for bounds in real {
            assert!(
                actual.max_x() <= bounds.origin.x
                    || actual.min_x() >= bounds.origin.x + bounds.size.width
                    || actual.max_y() <= bounds.origin.y
                    || actual.min_y() >= bounds.origin.y + bounds.size.height
            );
        }
        parking.restore(window.id).unwrap();
        wait_display(parked.display, false).unwrap();
        assert!(same_frame(
            parking.state().unwrap().quartz(window.id).unwrap().0.frame,
            window.frame
        ));
        parking.restore(window.id).unwrap();
        assert!(parking.recover().unwrap().is_empty());
        fs::remove_file(path).unwrap();
        println!("private_vdisplay_live: 1 passed");
    }

    /// Lead-attended only; compiling this body never opens a window or creates a display.
    #[allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
    pub(crate) fn live_textedit_fullscreen() {
        use crosspane_platform::WindowSource;
        let windows = crate::windows::MacWindows::new().unwrap();
        let window = windows
            .windows()
            .unwrap()
            .into_iter()
            .find(|w| w.app_id == "com.apple.TextEdit")
            .expect("lead must open disposable TextEdit window");
        let path = std::env::temp_dir().join(format!(
            "crosspane-twin-fullscreen-{}.journal",
            std::process::id()
        ));
        let mut parking = MacTwinParking::new(path.clone()).unwrap();
        let normal = parking
            .park(window.id, PixelSize::new(1600, 1200), 2.0)
            .unwrap();
        assert!(!normal.fullscreen);
        parking.set_fullscreen(window.id, true).unwrap();
        parking.set_fullscreen(window.id, true).unwrap();
        let fullscreen = parking.geometry(window.id).unwrap();
        assert!(fullscreen.fullscreen);
        let (_, bounds, pixels) = parking.state().unwrap().twin_metrics(window.id).unwrap();
        let raw = parking.state().unwrap().quartz(window.id).unwrap().0;
        assert!(raw.on_screen && bounds_equal(raw.frame, bounds));
        assert_eq!(fullscreen.content, content(bounds, bounds, pixels).unwrap());
        let resized = parking
            .resize(window.id, PixelSize::new(1920, 1080), 2.0)
            .unwrap();
        assert!(resized.fullscreen);
        assert_eq!(resized.display, normal.display);
        assert_eq!(
            resized.content,
            PixelRect::new(euclid::Point2D::new(0, 0), euclid::Point2D::new(1920, 1080))
        );
        assert_eq!(parking.geometry(window.id).unwrap(), resized);
        let (_, bounds, _) = parking.state().unwrap().twin_metrics(window.id).unwrap();
        let raw = parking.state().unwrap().quartz(window.id).unwrap().0;
        assert!(raw.on_screen && bounds_equal(raw.frame, bounds));
        parking.set_fullscreen(window.id, false).unwrap();
        parking.set_fullscreen(window.id, false).unwrap();
        assert!(!parking.geometry(window.id).unwrap().fullscreen);
        parking.restore(window.id).unwrap();
        wait_display(normal.display, false).unwrap();
        assert!(same_frame(
            parking.state().unwrap().quartz(window.id).unwrap().0.frame,
            window.frame
        ));
        assert!(parking.recover().unwrap().is_empty());
        assert_eq!(read_journal(&path).unwrap(), BTreeMap::new());
        fs::remove_file(path).unwrap();
        println!("private_vdisplay_fullscreen: 1 passed");
    }
}
