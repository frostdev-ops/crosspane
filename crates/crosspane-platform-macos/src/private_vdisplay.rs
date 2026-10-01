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

use std::cell::RefCell;
use std::collections::BTreeMap;
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
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Method, Sel};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CGSize};
use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration,
    CGConfigureDisplayOrigin, CGConfigureOption, CGDisplayBounds, CGDisplayCopyAllDisplayModes,
    CGDisplayCopyDisplayMode, CGDisplayMode, CGDisplaySetDisplayMode, CGError,
    CGGetActiveDisplayList, kCGDisplayShowDuplicateLowResolutionModes,
};
use objc2_foundation::{NSArray, NSString};

use crate::main_thread::{on_main, spawn_on_main};
use crate::windows::{AxWindow, RawWindow, WindowQuery, require_accessibility, valid_frame};

const MAIN_WAIT: Duration = Duration::from_secs(2);
const AX_WAIT: Duration = Duration::from_secs(2);
const DISPLAY_WAIT: Duration = Duration::from_secs(1);
const MAX_PIXELS: u32 = 8192;

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
                let _: () = msg_send![&*descriptor, setProductID: 0xC001_u32];
                let _: () = msg_send![&*descriptor, setVendorID: 0xF05D_u32];
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
}

/// M2 parking. Only numeric display handles cross the main-thread boundary.
/// All WindowParking calls must run on a worker thread, never the AppKit main thread:
/// the serial state mutex spans bounded mode/placement waits that need the main queue.
#[derive(Debug)]
pub struct MacTwinParking {
    // ponytail: one serial lock across native waits; use a command worker if concurrency matters.
    state: Mutex<TwinState>,
}

#[derive(Debug)]
struct TwinState {
    journal: PathBuf,
    entries: BTreeMap<WindowId, Entry>,
    displays: BTreeMap<WindowId, VirtualDisplay>,
    query: WindowQuery,
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
                entries,
                displays: BTreeMap::new(),
                query: WindowQuery::new()?,
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
    fn window(&self, window: WindowId) -> Result<RawWindow, PlatformError> {
        let pid = self.entries.get(&window).map(|entry| entry.pid);
        self.query
            .list(true)?
            .into_iter()
            .find(|raw| raw.id == window && pid.is_none_or(|pid| raw.pid == pid))
            .ok_or(PlatformError::NotFound)
    }

    fn remove_entry(&mut self, window: WindowId) -> Result<(), PlatformError> {
        let mut entries = self.entries.clone();
        entries.remove(&window);
        write_journal(&self.journal, &entries)?;
        self.entries = entries;
        Ok(())
    }

    fn restore_frame(&self, window: WindowId) -> Result<bool, PlatformError> {
        let Some(entry) = self.entries.get(&window) else {
            return Ok(false);
        };
        let raw = match self.window(window) {
            Ok(raw) => raw,
            Err(PlatformError::NotFound) => return Ok(false),
            Err(error) => return Err(error),
        };
        require_accessibility()?;
        write_journal(&self.journal, &self.entries)?;
        let ax = AxWindow::find(&raw, Instant::now() + AX_WAIT)?;
        ax.restore(entry.frame)?;
        let mut actual = ax.frame()?;
        if (actual.size.width - entry.frame.size.width).abs() > 2.0
            || (actual.size.height - entry.frame.size.height).abs() > 2.0
        {
            // AXSize was written on the twin first and may have been clamped there.
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
        if !self.entries.contains_key(&window) {
            if let Some(display) = self.displays.remove(&window) {
                display.release()?;
            }
            return Ok(false);
        }
        let restored = self.restore_frame(window);
        // Release even if AX restoration failed or permission was revoked; retain the journal.
        let released = self
            .displays
            .remove(&window)
            .map(VirtualDisplay::release)
            .transpose();
        if let Err(error) = released.as_ref() {
            tracing::warn!(%error, "twin release failed; cleanup queued on main thread, journal retained");
        }
        let restored = restored?;
        released?;
        self.remove_entry(window)?;
        Ok(restored)
    }

    fn abort(&mut self, window: WindowId, error: PlatformError) -> PlatformError {
        match self.restore(window) {
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

    fn park(&mut self, window: WindowId, mode: Mode) -> Result<Parked, PlatformError> {
        if self.displays.contains_key(&window) {
            return self.resize(window, mode);
        }
        require_accessibility()?;
        let raw = self.window(window)?;
        let ax = AxWindow::find(&raw, Instant::now() + AX_WAIT)?;
        let original = ax.frame()?;
        let inserted = !self.entries.contains_key(&window);
        self.entries.entry(window).or_insert(Entry {
            pid: raw.pid,
            frame: original,
        });
        write_journal(&self.journal, &self.entries)?;
        let setup = (|| {
            let display = VirtualDisplay::create(mode)?;
            if let Err(error) = wait_mode(display.id, mode) {
                if let Err(cleanup) = display.release() {
                    tracing::warn!(%cleanup, "twin setup cleanup queued on main thread");
                }
                return Err(error);
            }
            Ok(display)
        })();
        let display = match setup {
            Ok(display) => display,
            Err(error) => {
                if inserted {
                    self.remove_entry(window)?;
                }
                return Err(error);
            }
        };
        let id = display.id;
        self.displays.insert(window, display);
        let result = (|| {
            let placement = place_twin(id)?;
            // Use a fresh AX deadline after display creation; no expired message may move a window.
            let raw = self.window(window)?;
            let ax = AxWindow::find(&raw, Instant::now() + AX_WAIT)?;
            ax.restore(RectLogical::new(placement.frame.origin, mode.logical()))?;
            self.geometry(window)
        })();
        result.map_err(|error| self.abort(window, error))
    }

    fn resize(&mut self, window: WindowId, mode: Mode) -> Result<Parked, PlatformError> {
        if !self.displays.contains_key(&window) {
            return Err(PlatformError::NotFound);
        }
        let result = (|| {
            require_accessibility()?;
            write_journal(&self.journal, &self.entries)?;
            let display = self.displays.get(&window).ok_or(PlatformError::NotFound)?;
            display.apply(mode)?;
            wait_mode(display.id, mode)?;
            // Mode changes can alter the arrangement. Re-isolate before moving the window again.
            let placement = place_twin(display.id)?;
            let raw = self.window(window)?;
            let ax = AxWindow::find(&raw, Instant::now() + AX_WAIT)?;
            ax.restore(RectLogical::new(placement.frame.origin, mode.logical()))?;
            self.geometry(window)
        })();
        result.map_err(|error| self.abort(window, error))
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        let display = self.displays.get(&window).ok_or(PlatformError::NotFound)?;
        require_accessibility()?;
        let raw = self.window(window)?;
        let frame = AxWindow::find(&raw, Instant::now() + AX_WAIT)?.frame()?;
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
        Ok(Parked {
            window,
            kind: ParkingKind::Twin,
            display: display.id,
            content: content(frame, bounds, pixels)?,
        })
    }
}

impl WindowParking for MacTwinParking {
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
        result.map_err(|error| {
            if state.displays.contains_key(&window)
                && matches!(
                    error,
                    PlatformError::NotFound | PlatformError::PermissionDenied(_)
                )
            {
                state.abort(window, error)
            } else {
                error
            }
        })
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        self.state()?.restore(window).map(|_| ())
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        let mut state = self.state()?;
        let mut restored = Vec::new();
        let mut first_error = None;
        let windows: std::collections::BTreeSet<_> = state
            .entries
            .keys()
            .chain(state.displays.keys())
            .copied()
            .collect();
        for window in windows {
            match state.restore(window) {
                Ok(true) => restored.push(window),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "twin recovery failed; continuing with remaining windows");
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(restored),
        }
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
    let invalid = || PlatformError::Backend("invalid twin journal; retained for inspection".into());
    let mut lines = text.lines();
    if lines.next() != Some("crosspane-twin-v1") {
        return Err(invalid());
    }
    let mut entries = BTreeMap::new();
    for line in lines {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() != 6 {
            return Err(invalid());
        }
        let window = WindowId(parts[0].parse::<u64>().map_err(|_| invalid())?);
        let pid = parts[1].parse::<i32>().map_err(|_| invalid())?;
        let values = parts[2..]
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
            || entries.insert(window, Entry { pid, frame }).is_some()
        {
            return Err(invalid());
        }
    }
    Ok(entries)
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
            writeln!(
                file,
                "{} {} {} {} {} {}",
                window.0,
                entry.pid,
                entry.frame.origin.x,
                entry.frame.origin.y,
                entry.frame.size.width,
                entry.frame.size.height
            )
            .map_err(io_error)?;
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
        let actual = parking.state().unwrap().window(window.id).unwrap().frame;
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
            parking.state().unwrap().window(window.id).unwrap().frame,
            window.frame
        ));
        parking.restore(window.id).unwrap();
        assert!(parking.recover().unwrap().is_empty());
        fs::remove_file(path).unwrap();
        println!("private_vdisplay_live: 1 passed");
    }
}
