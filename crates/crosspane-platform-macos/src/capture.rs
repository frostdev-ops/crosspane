//! Session event-tap capture. Mutable input state stays on the tap run loop; abort
//! only touches atomics and public cursor APIs. No AppKit or private APIs.

use std::collections::HashSet;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use crosspane_platform::{
    CaptureAbort, CaptureEvent, CaptureId, CapturePortal, CaptureStart, Edge, EndReason, EventSink,
    InputCapture, IoGate, MotionKind, Permission, PermissionState, PlatformError, PortalId,
};
use crosspane_types::geom::{PointDevice, VectorLogical};
use crosspane_types::hid::{MouseButton, macos_to_hid};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use crosspane_types::time::MonoTime;
use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource,
    CFRunLoopSourceContext, CFRunLoopTimer, CFRunLoopTimerContext, CGPoint, CGRect,
    kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGAssociateMouseAndMouseCursorPosition, CGDisplayBounds, CGDisplayCopyDisplayMode,
    CGDisplayHideCursor, CGDisplayMode, CGDisplayShowCursor, CGError, CGEvent, CGEventField,
    CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CGGetDisplaysWithPoint, CGMouseButton,
    CGWarpMouseCursorPosition, kCGNullDirectDisplay,
};

use crate::{clock, permissions};

mod drag;

struct PreparedBegin {
    tap_ready: bool,
    tap: Option<CFRetained<CFMachPort>>,
    display: Display,
    location: CGPoint,
    held_keys: Vec<crosspane_types::hid::HidUsage>,
    locks: LockKeys,
}

fn drag_allowed(
    button: MouseButton,
    held: HeldButtons,
    detected: bool,
) -> Result<(), PlatformError> {
    if button != MouseButton::PRIMARY {
        return Err(PlatformError::Unsupported("begin_drag button"));
    }
    if !detected || !held.get(0) || (1..HeldButtons::CAPACITY).any(|n| held.get(n)) {
        return Err(PlatformError::PointerButtonHeld);
    }
    Ok(())
}

const CALL_BUDGET: Duration = Duration::from_millis(50);
const DELIVERY_LIMIT: usize = 4_096;
const INJECTED: i64 = 0x0043_5049_4E4A;
const SESSION: CGEventSourceStateID = CGEventSourceStateID::CombinedSessionState;
const PENDING: u64 = 1;
const EFFECTIVE: u64 = 2;
const RECOVERING: u64 = 3;
const UNKNOWN_TIME: u8 = 0;
const NANOSECONDS: u8 = 1;
const MACH_TICKS: u8 = 2;

// Apple's public HIToolbox/CarbonEventsCore.h, IsSecureEventInputEnabled(void).
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

// Apple's public CoreGraphics/CGEvent.h. The binding's callback uses NonNull, but disabled-tap
// notifications may have no event. Use the C pointer signature so NULL is checked before borrowing.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: CGEventTapLocation,
        place: CGEventTapPlacement,
        options: CGEventTapOptions,
        mask: u64,
        callback: unsafe extern "C-unwind" fn(
            CGEventTapProxy,
            CGEventType,
            *mut CGEvent,
            *mut c_void,
        ) -> *mut CGEvent,
        info: *mut c_void,
    ) -> *mut CFMachPort;
}

fn secure_input() -> bool {
    // SAFETY: public no-argument read of the session's Secure Event Input state.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

fn check_permissions() -> Result<(), PlatformError> {
    for permission in [Permission::InputMonitoring, Permission::Accessibility] {
        if permissions::state(permission) != PermissionState::Granted {
            return Err(PlatformError::PermissionDenied(permission));
        }
    }
    Ok(())
}

fn cg_result(result: CGError, operation: &'static str) -> Result<(), PlatformError> {
    if result == CGError::Success {
        Ok(())
    } else {
        Err(PlatformError::Backend(format!(
            "{operation}: CGError {}",
            result.0
        )))
    }
}

/// Pure decision; the caller supplies the tick conversion, without a baked-in timebase.
fn timestamp_interpretation(ts: u64, ticks_as_nanos: u64, now: u64) -> u8 {
    if ts == 0 {
        UNKNOWN_TIME
    } else if ts.abs_diff(now) < 1_000_000_000 {
        NANOSECONDS
    } else if ticks_as_nanos.abs_diff(now) < 1_000_000_000 {
        MACH_TICKS
    } else {
        UNKNOWN_TIME
    }
}

fn event_time(event: &CGEvent, interpretation: &AtomicU8) -> MonoTime {
    let ts = CGEvent::timestamp(Some(event));
    let now = clock::now();
    let converted = clock::from_ticks(ts);
    let mut mode = interpretation.load(Ordering::Acquire);
    // Our injector's manually constructed/stamped events cannot choose the real-event clock.
    if mode == UNKNOWN_TIME
        && CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData) != INJECTED
    {
        let detected = timestamp_interpretation(ts, converted.as_nanos(), now.as_nanos());
        if detected != UNKNOWN_TIME {
            match interpretation.compare_exchange(
                UNKNOWN_TIME,
                detected,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    mode = detected;
                    tracing::info!(
                        interpretation = if mode == NANOSECONDS {
                            "nanoseconds"
                        } else {
                            "mach ticks"
                        },
                        "selected CGEvent timestamp interpretation"
                    );
                }
                Err(cached) => mode = cached,
            }
        }
    }
    // Invalid events still fall back after the interpretation has been cached.
    match mode {
        NANOSECONDS if ts != 0 && ts.abs_diff(now.as_nanos()) < 1_000_000_000 => {
            MonoTime::from_nanos(ts)
        }
        MACH_TICKS if ts != 0 && converted.as_nanos().abs_diff(now.as_nanos()) < 1_000_000_000 => {
            converted
        }
        _ => now,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Display {
    id: DisplayId,
    bounds: CGRect,
    scale: f64,
}

impl Display {
    fn read(id: DisplayId) -> Result<Self, PlatformError> {
        let bounds = CGDisplayBounds(id.0);
        let mode = CGDisplayCopyDisplayMode(id.0).ok_or(PlatformError::NotFound)?;
        let pixels = CGDisplayMode::pixel_width(Some(&mode)) as f64;
        tracing::debug!(
            display = id.0,
            pixel_width = pixels,
            point_width = bounds.size.width,
            "capture display geometry"
        );
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 || pixels <= 0.0 {
            return Err(PlatformError::NotFound);
        }
        Ok(Self {
            id,
            bounds,
            scale: pixels / bounds.size.width,
        })
    }

    fn global(self, point: PointDevice) -> Result<CGPoint, PlatformError> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(PlatformError::Backend("non-finite warp position".into()));
        }
        Ok(CGPoint::new(
            self.bounds.origin.x + point.x / self.scale,
            self.bounds.origin.y + point.y / self.scale,
        ))
    }

    fn pin_point(self, point: CGPoint) -> Result<CGPoint, PlatformError> {
        let origin = self.bounds.origin;
        let size = self.bounds.size;
        if !point.x.is_finite()
            || !point.y.is_finite()
            || !origin.x.is_finite()
            || !origin.y.is_finite()
            || !size.width.is_finite()
            || !size.height.is_finite()
            || size.width <= 0.0
            || size.height <= 0.0
            || !self.scale.is_finite()
            || self.scale <= 0.0
        {
            return Err(PlatformError::Backend("invalid cursor pin geometry".into()));
        }
        // The far edge is outside the display. Keep the pin at least one device pixel inside it.
        let max_x = origin.x + (size.width - 1.0 / self.scale).max(0.0);
        let max_y = origin.y + (size.height - 1.0 / self.scale).max(0.0);
        Ok(CGPoint::new(
            point.x.clamp(origin.x, max_x),
            point.y.clamp(origin.y, max_y),
        ))
    }
}

/// Private cursor seam: tests replace every operation, including release display lookup.
struct CursorOps {
    associate: Box<dyn Fn(bool) -> Result<(), PlatformError> + Send + Sync>,
    hide: Box<dyn Fn() -> Result<(), PlatformError> + Send + Sync>,
    show: Box<dyn Fn() -> Result<(), PlatformError> + Send + Sync>,
    warp: Box<dyn Fn(CGPoint) -> Result<(), PlatformError> + Send + Sync>,
    display: Box<dyn Fn(DisplayId) -> Result<Display, PlatformError> + Send + Sync>,
}

impl CursorOps {
    fn native() -> Self {
        Self {
            associate: Box::new(|associated| {
                cg_result(
                    CGAssociateMouseAndMouseCursorPosition(associated),
                    if associated {
                        "associate cursor"
                    } else {
                        "detach cursor"
                    },
                )
            }),
            hide: Box::new(|| cg_result(CGDisplayHideCursor(kCGNullDirectDisplay), "hide cursor")),
            show: Box::new(|| cg_result(CGDisplayShowCursor(kCGNullDirectDisplay), "show cursor")),
            warp: Box::new(|point| cg_result(CGWarpMouseCursorPosition(point), "warp cursor")),
            display: Box::new(Display::read),
        }
    }
}

fn cursor_pin_enabled(value: Option<&str>) -> bool {
    value != Some("0")
}

#[derive(Clone, Copy)]
struct CursorPin {
    token: u64,
    epoch: u64,
    point: CGPoint,
}

#[derive(Default)]
struct PinMetrics {
    motions: AtomicU64,
    warps: AtomicU64,
    last_motion: AtomicU64,
    longest_gap: AtomicU64,
}

impl PinMetrics {
    fn reset(&self) {
        self.motions.store(0, Ordering::Release);
        self.warps.store(0, Ordering::Release);
        self.last_motion.store(0, Ordering::Release);
        self.longest_gap.store(0, Ordering::Release);
    }

    fn motion(&self, at: MonoTime) {
        self.motions.fetch_add(1, Ordering::Relaxed);
        let last = self.last_motion.swap(at.as_nanos(), Ordering::AcqRel);
        if last != 0 {
            self.longest_gap
                .fetch_max(at.as_nanos().saturating_sub(last), Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Portal {
    portal: CapturePortal,
    display: Display,
}

fn portal_hit(portal: Portal, point: CGPoint, dx: f64, dy: f64) -> Option<f64> {
    let bounds = portal.display.bounds;
    let x = point.x - bounds.origin.x;
    let y = point.y - bounds.origin.y;
    let (distance, along, outward) = match portal.portal.edge {
        Edge::Left => (x.abs(), y, dx < 0.0),
        Edge::Right => ((x - bounds.size.width).abs(), y, dx > 0.0),
        Edge::Top => (y.abs(), x, dy < 0.0),
        Edge::Bottom => ((y - bounds.size.height).abs(), x, dy > 0.0),
    };
    let device = along * portal.display.scale;
    (distance <= 1.0 && outward && device >= portal.portal.from && device <= portal.portal.to)
        .then(|| (device - portal.portal.from) / (portal.portal.to - portal.portal.from))
}

/// A pressed title-bar drag remains at the edge even when the OS pins its pointer.
fn drag_hit(portal: Portal, point: CGPoint) -> Option<f64> {
    let bounds = portal.display.bounds;
    let (distance, along) = match portal.portal.edge {
        Edge::Left => ((point.x - bounds.origin.x).abs(), point.y - bounds.origin.y),
        Edge::Right => (
            (point.x - bounds.origin.x - bounds.size.width).abs(),
            point.y - bounds.origin.y,
        ),
        Edge::Top => ((point.y - bounds.origin.y).abs(), point.x - bounds.origin.x),
        Edge::Bottom => (
            (point.y - bounds.origin.y - bounds.size.height).abs(),
            point.x - bounds.origin.x,
        ),
    };
    let device = along * portal.display.scale;
    (distance <= 1.0 && device >= portal.portal.from && device <= portal.portal.to)
        .then(|| (device - portal.portal.from) / (portal.portal.to - portal.portal.from))
}

/// Pointer buttons (numbers 0..=255) the tap has seen go down without a matching up. A cache of
/// what is believed held: `CGEventSource::button_state` is the authority and reconciles it
/// whenever it may be stale, since a disabled or timed-out tap can miss an up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HeldButtons([u64; 4]);

impl HeldButtons {
    const CAPACITY: usize = 256;

    fn get(&self, number: usize) -> bool {
        number < Self::CAPACITY && self.0[number / 64] & (1 << (number % 64)) != 0
    }

    fn set(&mut self, number: usize, down: bool) {
        if number < Self::CAPACITY {
            let bit = 1 << (number % 64);
            if down {
                self.0[number / 64] |= bit;
            } else {
                self.0[number / 64] &= !bit;
            }
        }
    }

    fn any(&self) -> bool {
        self.0.iter().any(|word| *word != 0)
    }
}

/// One non-injected pointer event, as far as E1 edge detection cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PointerInput {
    /// `MouseMoved`: no button was down when the OS made the event.
    Moved,
    /// `LeftMouseDragged`, `RightMouseDragged` or `OtherMouseDragged`: a button was down.
    Dragged,
    ButtonDown(usize),
    ButtonUp(usize),
}

/// What the tap emits for one pointer event, and the pressed set that follows it.
#[derive(Debug, Default, PartialEq)]
struct EdgeUpdate {
    /// `EdgePressed` for each portal under the pointer, with its position along the stretch.
    press: Vec<(PortalId, f64)>,
    /// `EdgeReleased`, in ascending portal order.
    release: Vec<PortalId>,
    /// The pressed set after this event.
    pressed: HashSet<PortalId>,
}

/// Pure E1 edge-eligibility decision; the tap feeds it and emits its result. A portal is pressed
/// only by pointer motion with no local button held, so a drag (a window resize, a selection, a
/// window move) never starts a crossing that `begin()` would refuse after the handshake work.
///
/// - `Dragged` is itself proof that a button is down. This also covers the instant between a
///   release and the tap seeing it, when `button_state` already reads "up" for a stale drag.
/// - `Moved` first reconciles the cache against `os_button_down`: a button whose up the tap missed
///   (disabled tap, timeout) is dropped, so it cannot block crossing for ever. The query runs only
///   when the cache is non-empty, which keeps the common motion path free of native calls.
/// - `ButtonDown` releases every pressed portal at once, cancelling any push-to-cross delay.
/// - `ButtonUp` never presses: a portal becomes eligible again only on later motion.
fn edge_update(
    input: PointerInput,
    buttons: &mut HeldButtons,
    os_button_down: impl Fn(usize) -> bool,
    pressed: &HashSet<PortalId>,
    hits: &[(PortalId, f64)],
) -> EdgeUpdate {
    let release_all = || {
        let mut release: Vec<_> = pressed.iter().copied().collect();
        release.sort();
        EdgeUpdate {
            release,
            ..EdgeUpdate::default()
        }
    };
    match input {
        PointerInput::ButtonDown(number) => {
            buttons.set(number, true);
            release_all()
        }
        PointerInput::ButtonUp(number) => {
            buttons.set(number, false);
            EdgeUpdate {
                pressed: pressed.clone(),
                ..EdgeUpdate::default()
            }
        }
        PointerInput::Dragged => release_all(),
        PointerInput::Moved => {
            if buttons.any() {
                for number in 0..HeldButtons::CAPACITY {
                    if buttons.get(number) && !os_button_down(number) {
                        buttons.set(number, false);
                    }
                }
                if buttons.any() {
                    return release_all();
                }
            }
            let now: HashSet<_> = hits.iter().map(|(id, _)| *id).collect();
            let mut release: Vec<_> = pressed.difference(&now).copied().collect();
            release.sort();
            EdgeUpdate {
                press: hits.to_vec(),
                release,
                pressed: now,
            }
        }
    }
}

fn modifier_down(keycode: u16, flags: CGEventFlags) -> Option<bool> {
    // Apple's public IOKit/hidsystem/IOLLEvent.h NX_DEVICE* masks distinguish held sides.
    let mask = match keycode {
        0x38 => 0x2,
        0x3c => 0x4,
        0x3b => 0x1,
        0x3e => 0x2000,
        0x3a => 0x20,
        0x3d => 0x40,
        0x37 => 0x8,
        0x36 => 0x10,
        0x3f => CGEventFlags::MaskSecondaryFn.bits(),
        _ => return None,
    };
    Some(flags.bits() & mask != 0)
}

fn button(number: i64) -> Option<MouseButton> {
    u8::try_from(number).ok()?.checked_add(1).map(MouseButton)
}

fn locks(flags: CGEventFlags) -> LockKeys {
    LockKeys {
        caps_lock: Some(flags.contains(CGEventFlags::MaskAlphaShift)),
        num_lock: None,
        scroll_lock: None,
    }
}

fn scroll(continuous: bool, x: f64, y: f64, phase: i64, momentum: i64) -> ScrollDelta {
    let phase = if !continuous {
        ScrollPhase::Discrete
    } else {
        match momentum {
            1 => ScrollPhase::MomentumBegan,
            2 => ScrollPhase::MomentumChanged,
            3 => ScrollPhase::MomentumEnded,
            _ if phase & 8 != 0 => ScrollPhase::Cancelled,
            _ if phase & 4 != 0 => ScrollPhase::Ended,
            _ if phase & 1 != 0 => ScrollPhase::Began,
            _ if phase & 2 != 0 => ScrollPhase::Changed,
            _ if phase & 128 != 0 => ScrollPhase::MayBegin,
            _ => ScrollPhase::Discrete,
        }
    };
    ScrollDelta {
        // ScrollDelta (crosspane-types/src/input.rs): +x scrolls right, +y scrolls up.
        // Quartz Axis2 has the opposite x sign; Axis1 already has the required y sign.
        // Natural scrolling is already applied. Smooth deltas stay in logical pixels.
        v120_x: if continuous { 0 } else { (-x * 120.0) as i32 },
        v120_y: if continuous { 0 } else { (y * 120.0) as i32 },
        pixels: continuous.then(|| VectorLogical::new(-x, y)),
        phase,
        stop_x: false,
        stop_y: false,
    }
}

struct Request {
    deadline: Instant,
    cancelled: AtomicBool,
    epoch: u64,
}

impl Request {
    fn valid(&self, shared: &Shared) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && Instant::now() < self.deadline
            && self.epoch == shared.epoch.load(Ordering::Acquire)
            && !shared.stop.load(Ordering::Acquire)
            && !shared.dead.load(Ordering::Acquire)
    }
}

enum Delivery {
    Subscribe(Arc<dyn EventSink<CaptureEvent>>, LockKeys, bool),
    Activate {
        token: u64,
        id: CaptureId,
        start: CaptureStart,
        request: Arc<Request>,
        reply: Sender<Result<CaptureStart, PlatformError>>,
    },
    Event(u64, CaptureEvent),
    End(u64, EndReason),
    Stop,
}

struct Shared {
    gate: Arc<IoGate>,
    /// Generation in the upper bits, PENDING/EFFECTIVE in the lower two; zero is idle.
    active: AtomicU64,
    capturing: AtomicBool,
    epoch: AtomicU64,
    cursor: CursorOps,
    pin_enabled: bool,
    /// Clearing this authorization invalidates the tap-owned coordinates before recovery.
    pin_token: AtomicU64,
    pin_metrics: PinMetrics,
    /// Token whose pin call is in flight; admission covers the final authorization check.
    pin_inflight: AtomicU64,
    /// Deferred finish: token in the upper bits, low bit requests the recorded release warp.
    pin_finish: AtomicU64,
    release_x: AtomicU64,
    release_y: AtomicU64,
    detached: AtomicBool,
    hidden: AtomicBool,
    stop: AtomicBool,
    dead: AtomicBool,
    output: SyncSender<Delivery>,
    time: AtomicU8,
    portals: Mutex<Arc<Vec<Portal>>>,
}

fn restore_flag(
    flag: &AtomicBool,
    restore: impl FnOnce() -> Result<(), PlatformError>,
) -> Result<(), PlatformError> {
    if flag.load(Ordering::Acquire) {
        restore()?;
        // A failed native restore remains outstanding for end, abort and Drop to retry.
        flag.store(false, Ordering::Release);
    }
    Ok(())
}

impl Shared {
    fn capture_active(&self, token: u64, epoch: u64) -> bool {
        let active = self.active.load(Ordering::SeqCst);
        active >> 2 == token
            && matches!(active & 3, PENDING | EFFECTIVE)
            && self.capturing.load(Ordering::Acquire)
            && self.epoch.load(Ordering::Acquire) == epoch
            && self.gate.is_open()
            && !self.stop.load(Ordering::Acquire)
            && !self.dead.load(Ordering::Acquire)
    }

    fn pin_active(&self, pin: CursorPin) -> bool {
        self.pin_token.load(Ordering::SeqCst) == pin.token
            && self.capture_active(pin.token, pin.epoch)
    }

    fn clear_pin(&self, token: u64) {
        if token != 0
            && self
                .pin_token
                .compare_exchange(token, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            tracing::debug!(
                token,
                warp_count = self.pin_metrics.warps.load(Ordering::Acquire),
                captured_motion_count = self.pin_metrics.motions.load(Ordering::Acquire),
                longest_motion_gap_ms =
                    self.pin_metrics.longest_gap.load(Ordering::Acquire) as f64 / 1_000_000.0,
                "capture cursor pin measurements"
            );
        }
    }

    fn complete_pin_finish(&self, token: u64) -> Result<(), PlatformError> {
        if self.pin_inflight.load(Ordering::SeqCst) == token {
            return Ok(());
        }
        let pending = self.pin_finish.load(Ordering::SeqCst);
        if pending == 0
            || pending >> 1 != token
            || self
                .pin_finish
                .compare_exchange(pending, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Ok(());
        }
        // Publication of pin_finish follows these coordinates. RECOVERING excludes a new
        // token until this final warp and show complete; neither writer nor reader needs a lock.
        let result = if pending & 1 != 0 {
            let point = CGPoint::new(
                f64::from_bits(self.release_x.load(Ordering::Acquire)),
                f64::from_bits(self.release_y.load(Ordering::Acquire)),
            );
            if self.gate.is_open() {
                (self.cursor.warp)(point)
            } else {
                Err(PlatformError::Locked)
            }
        } else {
            Ok(())
        };
        let shown = restore_flag(&self.hidden, || (self.cursor.show)());
        let _ = self.active.compare_exchange(
            token << 2 | RECOVERING,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        result.and(shown)
    }

    fn available(&self) -> Result<(), PlatformError> {
        if self.dead.load(Ordering::Acquire) || self.stop.load(Ordering::Acquire) {
            Err(PlatformError::Backend("capture worker stopped".into()))
        } else {
            Ok(())
        }
    }

    fn queue(&self, message: Delivery) -> Result<(), PlatformError> {
        if self.output.try_send(message).is_err() {
            // Overflow is terminal: release input immediately; delivery drops its backlog.
            // Mark dead before recovery so a failed End enqueue cannot recurse into recovery.
            if !self.dead.swap(true, Ordering::AcqRel) {
                self.fail();
            }
            Err(PlatformError::Backend(
                "capture delivery unavailable or full".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn fail(&self) {
        self.dead.store(true, Ordering::Release);
        let _ = self.finish(EndReason::Lost, None);
        // Wake an idle receiver; a full channel already has a wake pending.
        let _ = self.output.try_send(Delivery::Stop);
    }

    fn event(&self, token: u64, event: CaptureEvent) {
        if self.available().is_ok() {
            let _ = self.queue(Delivery::Event(token, event));
        }
    }

    fn restore_cursor(&self) -> Result<(), PlatformError> {
        let associated = restore_flag(&self.detached, || (self.cursor.associate)(true));
        let shown = restore_flag(&self.hidden, || (self.cursor.show)());
        associated.and(shown)
    }

    /// No application locks or drain wait: an in-flight pin defers the release immediately,
    /// preserving the total CALL_BUDGET. Abort/watchdog recovery never waits either.
    fn finish(
        &self,
        reason: EndReason,
        warp: Option<(DisplayId, PointDevice)>,
    ) -> Result<(), PlatformError> {
        self.finish_inner(reason, warp, None)
    }

    fn finish_token(&self, reason: EndReason, token: u64) -> Result<(), PlatformError> {
        self.finish_inner(reason, None, Some(token))
    }

    fn finish_inner(
        &self,
        reason: EndReason,
        warp: Option<(DisplayId, PointDevice)>,
        expected: Option<u64>,
    ) -> Result<(), PlatformError> {
        if expected.is_none() {
            self.epoch.fetch_add(1, Ordering::AcqRel);
        }
        let claimed = self
            .active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
                if active & 3 == RECOVERING || expected.is_some_and(|token| active >> 2 != token) {
                    None
                } else {
                    Some(active & !3 | RECOVERING)
                }
            });
        let active = match claimed {
            Ok(active) => active,
            Err(active) => {
                // A recovery may itself be stuck on the tap thread. The watchdog still
                // restores the cursor and delivers its end without waiting for that recovery.
                if active & 3 == RECOVERING && expected.is_none_or(|token| active >> 2 == token) {
                    self.capturing.store(false, Ordering::Release);
                    self.clear_pin(active >> 2);
                    let result = self.restore_cursor();
                    let _ = self.queue(Delivery::End(active >> 2, reason));
                    return result;
                }
                return Ok(());
            }
        };
        if expected.is_some() {
            self.epoch.fetch_add(1, Ordering::AcqRel);
        }
        self.capturing.store(false, Ordering::Release);
        self.clear_pin(active >> 2);
        let mut result = restore_flag(&self.detached, || (self.cursor.associate)(true));
        let mut release = None;
        if active != 0
            && let Some((display, point)) = warp
        {
            let warped = (self.cursor.display)(display)
                .and_then(|display| display.global(point))
                .and_then(|point| {
                    if self.gate.is_open() {
                        release = Some(point);
                        Ok(())
                    } else {
                        Err(PlatformError::Locked)
                    }
                });
            if result.is_ok() {
                result = warped;
            }
        }
        let token = active >> 2;
        if token != 0 && self.pin_inflight.load(Ordering::SeqCst) == token {
            // A blocked native pin cannot be cancelled. Keep RECOVERING until it drains and
            // hand the final release to the tap; no new capture can overtake either call.
            // Restore association and visibility before returning, even if the pin never drains.
            // Deferring immediately reserves the whole call budget for native cleanup.
            result = result.and(restore_flag(&self.hidden, || (self.cursor.show)()));
            if active & 3 == EFFECTIVE {
                let _ = self.queue(Delivery::End(token, reason));
            }
            if let Some(point) = release {
                self.release_x.store(point.x.to_bits(), Ordering::Release);
                self.release_y.store(point.y.to_bits(), Ordering::Release);
            }
            self.pin_finish
                .store(token << 1 | u64::from(release.is_some()), Ordering::SeqCst);
            // The pin can drain just before publication. Exactly one of this caller and the
            // tap claims the deferred finish. SeqCst publication/drain checks prevent both
            // sides from missing the other's store and leaving recovery unclaimed.
            let completed = self.complete_pin_finish(token);
            return result.and(completed).and(if release.is_some() {
                Err(PlatformError::Timeout)
            } else {
                Ok(())
            });
        }
        if let Some(point) = release {
            let warped = if self.gate.is_open() {
                (self.cursor.warp)(point)
            } else {
                Err(PlatformError::Locked)
            };
            result = result.and(warped);
        }
        let shown = restore_flag(&self.hidden, || (self.cursor.show)());
        result = result.and(shown);
        if active & 3 == EFFECTIVE {
            let _ = self.queue(Delivery::End(active >> 2, reason));
        }
        // Do not let another activation overlap recovery or overtake its Ended message.
        self.active.store(0, Ordering::Release);
        result
    }
}

struct Abort(Arc<Shared>);
impl CaptureAbort for Abort {
    fn abort(&self) {
        if let Err(error) = self.0.finish(EndReason::Aborted, None) {
            tracing::error!(%error, "capture abort cursor recovery failed");
        }
    }
}

struct DeliveryGuard(Arc<Shared>);
impl Drop for DeliveryGuard {
    fn drop(&mut self) {
        // Covers sink failures, channel closure, normal shutdown and unwinding.
        if self.0.stop.load(Ordering::Acquire) {
            let _ = self.0.finish(EndReason::Lost, None);
        } else {
            self.0.fail();
        }
    }
}

fn send_event(sink: &dyn EventSink<CaptureEvent>, event: CaptureEvent) -> bool {
    catch_unwind(AssertUnwindSafe(|| sink.send(event))).is_ok()
}

fn deliver(shared: Arc<Shared>, receiver: Receiver<Delivery>) {
    let _guard = DeliveryGuard(shared.clone());
    let mut sink: Option<Arc<dyn EventSink<CaptureEvent>>> = None;
    let mut active: Option<(u64, CaptureId)> = None;
    while let Ok(message) = receiver.recv() {
        if shared.dead.load(Ordering::Acquire) {
            // Safety notification bypasses the full queue. Dropping the receiver drops every
            // queued event/reply; none of that capture can follow this Ended.
            if let Some((_, id)) = active
                && let Some(sink) = &sink
            {
                let _ = send_event(
                    &**sink,
                    CaptureEvent::Ended {
                        id,
                        reason: EndReason::Lost,
                    },
                );
            }
            return;
        }
        match message {
            Delivery::Subscribe(new_sink, locks, blinded) => {
                if !send_event(&*new_sink, CaptureEvent::LockKeys(locks))
                    || !send_event(&*new_sink, CaptureEvent::KeyboardBlinded(blinded))
                {
                    return;
                }
                sink = Some(new_sink);
            }
            Delivery::Activate {
                token,
                id,
                start,
                request,
                reply,
            } => {
                let result = if request.valid(&shared)
                    && shared.gate.is_open()
                    && shared
                        .active
                        .compare_exchange(
                            token << 2 | PENDING,
                            token << 2 | EFFECTIVE,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                {
                    if let Some(sink) = &sink {
                        active = Some((token, id));
                        if !send_event(&**sink, CaptureEvent::Started { id }) {
                            shared.fail();
                            let _ = reply
                                .send(Err(PlatformError::Backend("capture sink panicked".into())));
                            return;
                        }
                        if request.valid(&shared)
                            && shared.gate.is_open()
                            && shared.active.load(Ordering::Acquire) == token << 2 | EFFECTIVE
                        {
                            Ok(start)
                        } else {
                            Err(PlatformError::Backend(
                                "capture lost during activation".into(),
                            ))
                        }
                    } else {
                        Err(PlatformError::Backend("capture has no subscription".into()))
                    }
                } else {
                    Err(if !shared.gate.is_open() {
                        PlatformError::Locked
                    } else {
                        PlatformError::Timeout
                    })
                };
                if result.is_err() {
                    let _ = shared.finish_token(EndReason::Lost, token);
                }
                let _ = reply.send(result);
            }
            Delivery::Event(token, event) => {
                // Abort may overtake a callback in progress; it cannot emit after Ended.
                if (token == 0 || active.is_some_and(|(current, _)| current == token))
                    && let Some(sink) = &sink
                    && !send_event(&**sink, event)
                {
                    return;
                }
            }
            Delivery::End(token, reason) => {
                if let Some((current, id)) = active
                    && current == token
                {
                    if let Some(sink) = &sink
                        && !send_event(&**sink, CaptureEvent::Ended { id, reason })
                    {
                        return;
                    }
                    active = None;
                }
            }
            Delivery::Stop => break,
        }
    }
}

struct Wake {
    run_loop: CFRetained<CFRunLoop>,
    source: CFRetained<CFRunLoopSource>,
}
// SAFETY: only public cross-thread CFRunLoopWakeUp/CFRunLoopSourceSignal are called;
// callbacks and mutation stay on the owning run loop, retained references preserve lifetime.
unsafe impl Send for Wake {}
// SAFETY: concurrent callers only signal/wake; they never access callback state.
unsafe impl Sync for Wake {}
impl Wake {
    fn wake(&self) {
        self.source.signal();
        self.run_loop.wake_up();
    }
}

enum Command {
    Subscribe(
        Arc<dyn EventSink<CaptureEvent>>,
        Arc<Request>,
        Sender<Result<(), PlatformError>>,
    ),
    Begin(
        CaptureId,
        PortalId,
        u64,
        Arc<Request>,
        Sender<Result<CaptureStart, PlatformError>>,
    ),
    BeginDrag(
        CaptureId,
        PortalId,
        u64,
        Arc<Request>,
        Sender<Result<CaptureStart, PlatformError>>,
    ),
    Monitor(bool, Arc<Request>, Sender<Result<(), PlatformError>>),
}

impl Command {
    fn fail(self) {
        let error = PlatformError::Backend("capture worker stopped".into());
        match self {
            Self::Subscribe(_, _, reply) | Self::Monitor(_, _, reply) => {
                let _ = reply.send(Err(error));
            }
            Self::Begin(_, _, _, _, reply) | Self::BeginDrag(_, _, _, _, reply) => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

/// One suppressing session event tap. Construction checks both TCC grants without prompts.
pub struct MacCapture {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    wake: Arc<Wake>,
    next_token: u64,
}
impl std::fmt::Debug for MacCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacCapture")
            .field("capturing", &self.shared.capturing.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl MacCapture {
    /// Fails with PermissionDenied(InputMonitoring | Accessibility) if either is missing.
    pub fn new(gate: Arc<IoGate>) -> Result<MacCapture, PlatformError> {
        check_permissions()?;
        let (output, events) = mpsc::sync_channel(DELIVERY_LIMIT);
        let shared = Arc::new(Shared {
            gate,
            output,
            active: AtomicU64::new(0),
            capturing: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            cursor: CursorOps::native(),
            pin_enabled: cursor_pin_enabled(
                std::env::var("CROSSPANE_MAC_CURSOR_PIN").ok().as_deref(),
            ),
            pin_token: AtomicU64::new(0),
            pin_metrics: PinMetrics::default(),
            pin_inflight: AtomicU64::new(0),
            pin_finish: AtomicU64::new(0),
            release_x: AtomicU64::new(0),
            release_y: AtomicU64::new(0),
            detached: AtomicBool::new(false),
            hidden: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            time: AtomicU8::new(UNKNOWN_TIME),
            portals: Mutex::new(Arc::new(Vec::new())),
        });
        let delivery_shared = shared.clone();
        std::thread::Builder::new()
            .name("mac-capture-events".into())
            .spawn(move || deliver(delivery_shared, events))
            .map_err(|error| PlatformError::Backend(format!("spawn capture delivery: {error}")))?;
        let (commands, receiver) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let tap_shared = shared.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("mac-capture-tap".into())
            .spawn(move || run_tap(tap_shared, receiver, ready))
        {
            let _ = shared.queue(Delivery::Stop);
            return Err(PlatformError::Backend(format!(
                "spawn capture tap: {error}"
            )));
        }
        match started.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(wake)) => Ok(Self {
                shared,
                commands,
                wake,
                next_token: 1,
            }),
            result => {
                shared.stop.store(true, Ordering::Release);
                let _ = shared.queue(Delivery::Stop);
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(PlatformError::Timeout),
                }
            }
        }
    }

    fn request(&self) -> Arc<Request> {
        Arc::new(Request {
            deadline: Instant::now() + CALL_BUDGET,
            cancelled: AtomicBool::new(false),
            epoch: self.shared.epoch.load(Ordering::Acquire),
        })
    }

    fn wait<T>(
        &self,
        receiver: Receiver<Result<T, PlatformError>>,
        request: &Request,
    ) -> Result<T, PlatformError> {
        self.wake.wake();
        match receiver.recv_timeout(request.deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(error) => {
                request.cancelled.store(true, Ordering::Release);
                Err(match error {
                    mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
                    mpsc::RecvTimeoutError::Disconnected => {
                        PlatformError::Backend("capture thread stopped".into())
                    }
                })
            }
        }
    }

    fn send(&self, command: Command) -> Result<(), PlatformError> {
        self.shared.available()?;
        self.commands
            .send(command)
            .map_err(|_| PlatformError::Backend("capture thread stopped".into()))
    }
}

impl InputCapture for MacCapture {
    fn set_portals(&mut self, portals: &[CapturePortal]) -> Result<(), PlatformError> {
        self.shared.available()?;
        let request = self.request();
        let mut ids = HashSet::new();
        let mut prepared = Vec::with_capacity(portals.len());
        for &portal in portals {
            if Instant::now() >= request.deadline {
                return Err(PlatformError::Timeout);
            }
            if !portal.from.is_finite()
                || !portal.to.is_finite()
                || portal.from < 0.0
                || portal.from >= portal.to
                || !ids.insert(portal.id)
            {
                return Err(PlatformError::Backend("invalid capture portal".into()));
            }
            let display = Display::read(portal.display)?;
            let length = match portal.edge {
                Edge::Left | Edge::Right => display.bounds.size.height,
                Edge::Top | Edge::Bottom => display.bounds.size.width,
            } * display.scale;
            if portal.to > length {
                return Err(PlatformError::Backend("portal exceeds display edge".into()));
            }
            prepared.push(Portal { portal, display });
        }
        let prepared = Arc::new(prepared);
        loop {
            if Instant::now() >= request.deadline {
                return Err(PlatformError::Timeout);
            }
            match self.shared.portals.try_lock() {
                Ok(mut current) => {
                    *current = prepared;
                    break;
                }
                Err(TryLockError::WouldBlock) => std::thread::yield_now(),
                Err(TryLockError::Poisoned(_)) => {
                    return Err(PlatformError::Backend("portal store poisoned".into()));
                }
            }
        }
        // Publication is the commit. A failed call never changes the previous set, and
        // there is no queued mutation that can run after a timeout.
        self.wake.wake();
        Ok(())
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<CaptureEvent>>) -> Result<(), PlatformError> {
        let request = self.request();
        let (reply, receiver) = mpsc::channel();
        self.send(Command::Subscribe(sink, request.clone(), reply))?;
        self.wait(receiver, &request)
    }

    fn begin(&mut self, id: CaptureId, portal: PortalId) -> Result<CaptureStart, PlatformError> {
        // Preserve the frozen refusal order even if the tap thread is unavailable.
        if !self.shared.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        self.shared.available()?;
        if secure_input() {
            return Err(PlatformError::SecureInput);
        }
        if (0..=4).any(|b| CGEventSource::button_state(SESSION, CGMouseButton(b))) {
            return Err(PlatformError::PointerButtonHeld);
        }
        let request = self.request();
        let token = self.next_token;
        self.next_token = token
            .checked_add(1)
            .filter(|value| *value <= u64::MAX >> 2)
            .ok_or_else(|| PlatformError::Backend("capture generation exhausted".into()))?;
        let (reply, receiver) = mpsc::channel();
        self.send(Command::Begin(id, portal, token, request.clone(), reply))?;
        let result = self.wait(receiver, &request);
        if result.is_err() {
            let _ = self.shared.finish_token(EndReason::Lost, token);
        }
        result
    }

    fn end(&mut self, warp_to: Option<(DisplayId, PointDevice)>) -> Result<(), PlatformError> {
        // Release first; destination lookup/validation failures still show the cursor and end.
        self.shared.finish(EndReason::Requested, warp_to)
    }

    fn begin_drag(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        button: MouseButton,
    ) -> Result<CaptureStart, PlatformError> {
        if button != MouseButton::PRIMARY {
            return Err(PlatformError::Unsupported("begin_drag button"));
        }
        if !self.shared.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        self.shared.available()?;
        if secure_input() {
            return Err(PlatformError::SecureInput);
        }
        let request = self.request();
        let token = self.next_token;
        self.next_token = token
            .checked_add(1)
            .filter(|value| *value <= u64::MAX >> 2)
            .ok_or_else(|| PlatformError::Backend("capture generation exhausted".into()))?;
        let (reply, receiver) = mpsc::channel();
        self.send(Command::BeginDrag(
            id,
            portal,
            token,
            request.clone(),
            reply,
        ))?;
        let result = self.wait(receiver, &request);
        if result.is_err() {
            let _ = self.shared.finish_token(EndReason::Lost, token);
        }
        result
    }

    fn abort_handle(&self) -> Arc<dyn CaptureAbort> {
        Arc::new(Abort(self.shared.clone()))
    }

    fn set_monitor_local_activity(&mut self, on: bool) -> Result<(), PlatformError> {
        let request = self.request();
        let (reply, receiver) = mpsc::channel();
        self.send(Command::Monitor(on, request.clone(), reply))?;
        self.wait(receiver, &request)
    }
}

impl Drop for MacCapture {
    fn drop(&mut self) {
        let _ = self.shared.finish(EndReason::Aborted, None);
        self.shared.stop.store(true, Ordering::Release);
        self.wake.wake();
        let _ = self.shared.queue(Delivery::Stop);
        // No join: drop/watchdog must work even if the tap is stuck.
    }
}

struct TapState {
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    tap: Option<CFRetained<CFMachPort>>,
    portal_config: Arc<Vec<Portal>>,
    portals: Arc<Vec<Portal>>,
    pressed: HashSet<PortalId>,
    subscribed: bool,
    monitor: bool,
    last_activity: Option<MonoTime>,
    last_secure_poll: Instant,
    last_geometry_poll: Instant,
    blinded: bool,
    lock_keys: LockKeys,
    display: Option<Display>,
    pin: Option<CursorPin>,
    local_keys: [bool; 128],
    suppressed_keys: [u64; 128],
    suppressed_buttons: [u64; 256],
    /// Every button the tap saw go down and not yet up, whether or not capture suppressed it.
    held_buttons: HeldButtons,
    drag: drag::Move,
    window_at: Box<dyn Fn(CGPoint) -> Result<Option<drag::WindowFact>, PlatformError>>,
    tiling_busy: Arc<AtomicBool>,
    settled_primary: bool,
    settled_capture: u64,
    settled_tail_until: Option<Instant>,
}

impl TapState {
    fn start_pin(
        &mut self,
        token: u64,
        epoch: u64,
        display: Display,
        location: CGPoint,
    ) -> Result<(), PlatformError> {
        self.pin = None;
        if !self.shared.pin_enabled {
            return Ok(());
        }
        let pin = CursorPin {
            token,
            epoch,
            point: display.pin_point(location)?,
        };
        if !self.shared.capture_active(token, epoch) {
            return Err(PlatformError::Timeout);
        }
        self.shared.pin_metrics.reset();
        self.shared.pin_token.store(token, Ordering::SeqCst);
        // Abort can overtake setup. Never publish coordinates for its ended generation.
        if !self.shared.pin_active(pin) {
            self.shared.clear_pin(token);
            return Err(PlatformError::Timeout);
        }
        self.pin = Some(pin);
        Ok(())
    }

    fn pin_motion(&mut self, token: u64, location: CGPoint) {
        let Some(pin) = self.pin else { return };
        if pin.token != token || !self.shared.pin_active(pin) {
            self.pin = None;
            self.shared.clear_pin(pin.token);
            return;
        }
        // Measure tap arrival gaps, rather than remote deltas or synthetic warp positions.
        self.shared.pin_metrics.motion(clock::now());
        if location != pin.point
            && self
                .shared
                .pin_inflight
                .compare_exchange(0, token, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            // Recovery either observes this guard and waits/defers its release, or precedes
            // this final check, in which case no native pin is submitted. Admission, authorization
            // clearing, and their checks share a SeqCst order, so neither side can miss the other.
            if self.shared.pin_active(pin) {
                // Plain public warp, once per swallowed motion. Suppression is measured by the lead.
                self.shared
                    .pin_metrics
                    .warps
                    .fetch_add(1, Ordering::Relaxed);
                if let Err(error) = (self.shared.cursor.warp)(pin.point) {
                    tracing::error!(token, %error, "capture cursor pin disabled after warp failure");
                    self.shared.clear_pin(token);
                    self.pin = None;
                }
            }
            self.shared.pin_inflight.store(0, Ordering::SeqCst);
            if let Err(error) = self.shared.complete_pin_finish(token) {
                tracing::error!(%error, "capture deferred cursor recovery failed");
            }
        }
    }

    fn sync_portals(&mut self) {
        // The callback never waits for a writer. Immutable snapshots keep hit testing fast.
        let portals = match self.shared.portals.try_lock() {
            Ok(current) => current.clone(),
            Err(_) => return,
        };
        if Arc::ptr_eq(&self.portal_config, &portals) {
            return;
        }
        let at = clock::now();
        self.pressed.retain(|id| {
            let old = self.portals.iter().find(|p| p.portal.id == *id);
            let new = portals.iter().find(|p| p.portal.id == *id);
            let keep = old
                .zip(new)
                .is_some_and(|(old, new)| old.portal == new.portal && old.display == new.display);
            if !keep {
                self.shared
                    .event(0, CaptureEvent::EdgeReleased { portal: *id, at });
            }
            keep
        });
        self.portal_config = portals.clone();
        self.portals = portals;
        for event in self.drag.clear(at) {
            self.shared.event(0, event);
        }
    }

    fn release_edges(&mut self, at: MonoTime) {
        self.release_plain_edges(at);
        for event in self.drag.clear(at) {
            self.shared.event(0, event);
        }
    }

    fn release_plain_edges(&mut self, at: MonoTime) {
        for portal in self.pressed.drain() {
            self.shared
                .event(0, CaptureEvent::EdgeReleased { portal, at });
        }
    }

    fn begin(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        token: u64,
        dragging: bool,
        request: Arc<Request>,
        reply: Sender<Result<CaptureStart, PlatformError>>,
    ) {
        self.sync_portals();
        self.begin_with(id, token, request, reply, |state, request| {
            state.prepare_begin(portal, token, request, dragging)
        });
    }

    /// The complete production commit/rollback path, with native preparation injected by the
    /// caller so fake tests exercise recovery and delivery rather than manually repairing state.
    fn begin_with(
        &mut self,
        id: CaptureId,
        token: u64,
        request: Arc<Request>,
        reply: Sender<Result<CaptureStart, PlatformError>>,
        prepare: impl FnOnce(&mut Self, &Request) -> Result<CaptureStart, PlatformError>,
    ) {
        let result = prepare(self, &request);
        match result {
            Ok(start) => {
                let _ = self.shared.queue(Delivery::Activate {
                    token,
                    id,
                    start,
                    request,
                    reply,
                });
            }
            Err(error) => {
                let active = self.shared.active.load(Ordering::Acquire);
                if active >> 2 == token || active == 0 {
                    self.shared.capturing.store(false, Ordering::Release);
                    let _ = self.shared.finish_token(EndReason::Lost, token);
                    // Abort may have overtaken an in-flight native setup call. Repair its
                    // completed resources even if that abort already cleared the generation.
                    let _ = self.shared.restore_cursor();
                }
                let _ = reply.send(Err(error));
            }
        }
    }

    fn prepare_begin(
        &mut self,
        portal: PortalId,
        token: u64,
        request: &Request,
        dragging: bool,
    ) -> Result<CaptureStart, PlatformError> {
        if !request.valid(&self.shared) {
            return Err(PlatformError::Timeout);
        }
        if !self.shared.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if secure_input() {
            return Err(PlatformError::SecureInput);
        }
        if !dragging && (0..=4).any(|b| CGEventSource::button_state(SESSION, CGMouseButton(b))) {
            return Err(PlatformError::PointerButtonHeld);
        }
        check_permissions()?;
        if !self.subscribed {
            return Err(PlatformError::Backend("subscribe before capture".into()));
        }
        if self.shared.active.load(Ordering::Acquire) != 0 {
            return Err(PlatformError::Backend("capture already active".into()));
        }
        let portal = self
            .portals
            .iter()
            .find(|p| p.portal.id == portal)
            .ok_or(PlatformError::NotFound)?;
        let dragged = if dragging {
            let mut held = self.held_buttons;
            // Dragged proves left was held even if setup missed its down; refresh every other
            // known held button, plus the standard buttons, before authorizing settlement.
            for number in 0..HeldButtons::CAPACITY {
                if number <= 4 || held.get(number) {
                    held.set(
                        number,
                        CGEventSource::button_state(SESSION, CGMouseButton(number as u32)),
                    );
                }
            }
            drag_allowed(
                MouseButton::PRIMARY,
                held,
                self.drag.at_edge(portal.portal.id).is_some(),
            )?;
            self.drag.at_edge(portal.portal.id)
        } else {
            None
        };
        // Refresh geometry at activation, rather than assuming the portal's display is unchanged.
        let display = Display::read(portal.display.id)?;
        let current = CGEvent::new(None)
            .ok_or_else(|| PlatformError::Backend("read frozen cursor location".into()))?;
        let location = CGEvent::location(Some(&current));
        let up = if let Some((window, _)) = dragged {
            if drag::distance(*portal, location) > 1.0
                || (location.x - self.drag.pointer.x).hypot(location.y - self.drag.pointer.y) > 1.0
                || !(self.window_at)(location)?
                    .is_some_and(|now| now.window == window.window && now.pid == window.pid)
            {
                return Err(PlatformError::PointerButtonHeld);
            }
            let up = CGEvent::new_mouse_event(
                None,
                CGEventType::LeftMouseUp,
                location,
                CGMouseButton::Left,
            )
            .ok_or_else(|| PlatformError::Backend("create drag settlement up".into()))?;
            CGEvent::set_integer_value_field(
                Some(&up),
                CGEventField::EventSourceUserData,
                INJECTED,
            );
            Some(up)
        } else {
            None
        };
        let display = if location.x >= display.bounds.origin.x
            && location.x <= display.bounds.origin.x + display.bounds.size.width
            && location.y >= display.bounds.origin.y
            && location.y <= display.bounds.origin.y + display.bounds.size.height
        {
            display
        } else {
            let mut id = 0;
            let mut count = 0;
            cg_result(
                // SAFETY: buffers hold the one display ID and count requested by this read-only call.
                unsafe { CGGetDisplaysWithPoint(location, 1, &mut id, &mut count) },
                "locate frozen cursor display",
            )?;
            if count == 0 {
                return Err(PlatformError::NotFound);
            }
            Display::read(DisplayId(id))?
        };
        if !self
            .tap
            .as_ref()
            .is_some_and(|tap| CGEvent::tap_is_enabled(tap))
        {
            return Err(PlatformError::Backend("event tap unavailable".into()));
        }
        let mut held_keys = Vec::new();
        for keycode in 0..128u16 {
            let held = CGEventSource::key_state(SESSION, keycode);
            if !held {
                self.suppressed_keys[usize::from(keycode)] = 0;
            }
            self.local_keys[usize::from(keycode)] =
                held && self.suppressed_keys[usize::from(keycode)] == 0;
            if held && let Some(usage) = macos_to_hid(keycode) {
                held_keys.push(usage);
            }
        }
        self.lock_keys = locks(CGEventSource::flags_state(SESSION));
        let prepared = PreparedBegin {
            tap_ready: true,
            tap: self.tap.clone(),
            display,
            location,
            held_keys,
            locks: self.lock_keys,
        };
        let result = self.activate_begin(
            token,
            request,
            prepared,
            up.as_ref().map(|up| {
                move || {
                    CGEvent::post(CGEventTapLocation::SessionEventTap, Some(up));
                }
            }),
        );
        if result.is_ok()
            && let Some((window, before)) = dragged
        {
            if let Some(before) = before {
                drag::correct_tiling(window, before, display.id, self.tiling_busy.clone());
            } else {
                tracing::warn!(
                    window = window.window.0,
                    "drag tiling correction skipped: no observed pre-edge frame"
                );
            }
        }
        result
    }

    /// Shared native/fake activation sequence. The installed tap was validated by preparation;
    /// all fallible cursor setup precedes the irreversible session up. Delivery may still fail
    /// afterward, in which case the suppressed-button ledger outlives that capture generation.
    fn activate_begin(
        &mut self,
        token: u64,
        request: &Request,
        prepared: PreparedBegin,
        post_up: Option<impl FnOnce()>,
    ) -> Result<CaptureStart, PlatformError> {
        if !request.valid(&self.shared) {
            return Err(PlatformError::Timeout);
        }
        if !self.shared.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if !prepared.tap_ready {
            return Err(PlatformError::Backend("event tap unavailable".into()));
        }
        self.shared.restore_cursor()?;
        self.display = Some(prepared.display);
        if post_up.is_some() {
            // This crossing owns the detected drag. Keep it until settlement commits, and do
            // not emit a cancellation to the engine between its command and Started.
            self.release_plain_edges(clock::now());
        } else {
            self.release_edges(clock::now());
        }
        self.shared
            .active
            .compare_exchange(0, token << 2 | PENDING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| PlatformError::Backend("capture recovery in progress".into()))?;
        self.shared.capturing.store(true, Ordering::Release);
        if !request.valid(&self.shared) || !self.shared.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        self.start_pin(token, request.epoch, prepared.display, prepared.location)?;
        (self.shared.cursor.associate)(false)?;
        self.shared.detached.store(true, Ordering::Release);
        if !request.valid(&self.shared)
            || self.shared.active.load(Ordering::Acquire) != token << 2 | PENDING
        {
            let _ = self.shared.finish(EndReason::Lost, None);
            return Err(PlatformError::Timeout);
        }
        if !self.shared.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        (self.shared.cursor.hide)()?;
        self.shared.hidden.store(true, Ordering::Release);
        if !request.valid(&self.shared)
            || self.shared.active.load(Ordering::Acquire) != token << 2 | PENDING
            || !self.shared.gate.is_open()
        {
            let _ = self.shared.finish(EndReason::Lost, None);
            return Err(if self.shared.gate.is_open() {
                PlatformError::Timeout
            } else {
                PlatformError::Locked
            });
        }
        if let Some(post_up) = post_up {
            if prepared
                .tap
                .as_ref()
                .is_some_and(|tap| !CGEvent::tap_is_enabled(tap))
            {
                return Err(PlatformError::Backend(
                    "event tap unavailable before settlement".into(),
                ));
            }
            // Record suppression BEFORE posting, just as injection owns a down before exposing
            // it. This up owes no synthetic mate, including on panic/abort/activation timeout.
            self.suppressed_buttons[0] = token;
            self.held_buttons.set(0, true);
            self.settled_primary = true;
            self.settled_capture = token;
            self.settled_tail_until = None;
            self.drag = drag::Move::default();
            post_up();
            tracing::info!(
                token,
                "native window move settled: one session primary up posted"
            );
        }
        // Disassociation and hiding are best-effort foreground behavior (WP-1.19).
        // Background capture also pins swallowed motion with public warps on this tap thread.
        Ok(CaptureStart {
            held_keys: prepared.held_keys,
            lock_keys: prepared.locks,
        })
    }

    fn commands(&mut self) {
        if self.shared.dead.load(Ordering::Acquire) {
            while let Ok(command) = self.commands.try_recv() {
                command.fail();
            }
            return;
        }
        if self.shared.stop.load(Ordering::Acquire) {
            self.stop_when_released();
            return;
        }
        self.sync_portals();
        while let Ok(command) = self.commands.try_recv() {
            if self.shared.available().is_err() {
                command.fail();
                continue;
            }
            match command {
                Command::Subscribe(sink, request, reply) => {
                    let result = if !request.valid(&self.shared) {
                        Err(PlatformError::Timeout)
                    } else if self.subscribed {
                        Err(PlatformError::Backend(
                            "InputCapture::subscribe called twice".into(),
                        ))
                    } else {
                        self.blinded = secure_input();
                        self.lock_keys = locks(CGEventSource::flags_state(SESSION));
                        match self.shared.queue(Delivery::Subscribe(
                            sink,
                            self.lock_keys,
                            self.blinded,
                        )) {
                            Ok(()) => {
                                self.subscribed = true;
                                Ok(())
                            }
                            Err(_) => {
                                Err(PlatformError::Backend("capture delivery stopped".into()))
                            }
                        }
                    };
                    let _ = reply.send(result);
                }
                Command::Begin(id, portal, token, request, reply) => {
                    self.begin(id, portal, token, false, request, reply)
                }
                Command::BeginDrag(id, portal, token, request, reply) => {
                    self.begin(id, portal, token, true, request, reply)
                }
                Command::Monitor(on, request, reply) => {
                    let result = if request.valid(&self.shared) {
                        self.monitor = on;
                        Ok(())
                    } else {
                        Err(PlatformError::Timeout)
                    };
                    let _ = reply.send(result);
                }
            }
        }
    }

    fn poll(&mut self) {
        if self.shared.dead.load(Ordering::Acquire) {
            self.failed_callback();
            return;
        }
        // This timer runs every 50 ms, including while idle. Disabled taps are restored even
        // without input; gate revocation does not wait for the next physical event.
        if self.shared.capturing.load(Ordering::Acquire) && !self.shared.gate.is_open() {
            let _ = self.shared.finish(EndReason::Lost, None);
        }
        if self
            .tap
            .as_ref()
            .is_some_and(|tap| !CGEvent::tap_is_enabled(tap))
        {
            self.tap_disabled();
        }
        self.sync_portals();
        if self.last_geometry_poll.elapsed() >= Duration::from_secs(1) {
            self.last_geometry_poll = Instant::now();
            let refreshed: Vec<_> = self
                .portal_config
                .iter()
                .filter_map(|portal| {
                    Display::read(portal.portal.display)
                        .ok()
                        .map(|display| Portal { display, ..*portal })
                })
                .collect();
            if refreshed.len() != self.portals.len()
                || refreshed
                    .iter()
                    .zip(self.portals.iter())
                    .any(|(new, old)| new.display != old.display)
            {
                self.release_edges(clock::now());
                self.portals = Arc::new(refreshed);
            }
            if self.shared.capturing.load(Ordering::Acquire)
                && let Some(display) = self.display
            {
                match Display::read(display.id) {
                    Ok(current) if current == display => {}
                    _ => {
                        let _ = self.shared.finish(EndReason::Lost, None);
                    }
                }
            }
        }
        if self.last_secure_poll.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.last_secure_poll = Instant::now();
        let blinded = secure_input();
        if blinded != self.blinded {
            self.blinded = blinded;
            if blinded {
                let _ = self.shared.finish(EndReason::Lost, None);
            }
            if self.subscribed {
                self.shared.event(0, CaptureEvent::KeyboardBlinded(blinded));
            }
        }
        let current = locks(CGEventSource::flags_state(SESSION));
        if current != self.lock_keys {
            self.lock_keys = current;
            if self.subscribed {
                self.shared.event(0, CaptureEvent::LockKeys(current));
            }
        }
        if !self.shared.capturing.load(Ordering::Acquire) && !blinded {
            // A disabled/blinded tap may have missed a release. Once visibility is back,
            // reconcile only previously suppressed inputs so a later fresh down stays local.
            for (code, token) in self.suppressed_keys.iter_mut().enumerate() {
                if *token != 0 && !CGEventSource::key_state(SESSION, code as u16) {
                    *token = 0;
                }
            }
            self.reconcile_buttons(Instant::now(), |source, number| {
                CGEventSource::button_state(source, CGMouseButton(number as u32))
            });
        }
        if self.shared.stop.load(Ordering::Acquire) {
            // Dropping ends capture immediately, but the tap remains for suppressed ups.
            self.stop_when_released();
        }
    }

    fn reconcile_buttons(
        &mut self,
        now: Instant,
        mut held: impl FnMut(CGEventSourceStateID, usize) -> bool,
    ) {
        if self.settled_primary && self.suppressed_buttons[0] != 0 {
            let released = !held(CGEventSourceStateID::HIDSystemState, 0);
            // HID can report released before the session callback is queued. Keep ownership
            // until that callback consumes the up; one second bounds a genuinely missed up.
            self.pending_settled_tail(now, released);
        }
        for (number, token) in self.suppressed_buttons.iter_mut().enumerate() {
            if number == 0 && self.settled_primary {
                continue;
            }
            if *token != 0 && !held(SESSION, number) {
                *token = 0;
            }
        }
    }

    fn pending_settled_tail(&mut self, now: Instant, start_timeout: bool) -> bool {
        if !self.settled_primary || self.suppressed_buttons[0] == 0 {
            return false;
        }
        if start_timeout {
            self.settled_tail_until
                .get_or_insert(now + Duration::from_secs(1));
        }
        if self.settled_tail_until.is_some_and(|until| now >= until) {
            self.suppressed_buttons[0] = 0;
            self.settled_primary = false;
            self.settled_tail_until = None;
            return false;
        }
        true
    }

    /// Minimal terminal path: no delivery, capture, lookup or cursor work. Keep owning this
    /// irreversible physical tail while the existing tap lives, even if its consumer is dead.
    fn swallow_settled_tail(&mut self, kind: CGEventType, event: &CGEvent) -> bool {
        if !self.owns_settled_tail(kind, event) {
            return false;
        }
        if kind == CGEventType::LeftMouseUp {
            self.suppressed_buttons[0] = 0;
            self.settled_primary = false;
            self.settled_tail_until = None;
            self.held_buttons.set(0, false);
        }
        true
    }

    fn owns_settled_tail(&self, kind: CGEventType, event: &CGEvent) -> bool {
        self.settled_primary
            && self.suppressed_buttons[0] != 0
            && matches!(
                kind,
                CGEventType::LeftMouseDragged | CGEventType::LeftMouseUp
            )
            && CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData)
                != INJECTED
    }

    fn stop_when_released(&self) {
        if self.suppressed_keys.iter().all(|token| *token == 0)
            && self.suppressed_buttons.iter().all(|token| *token == 0)
            && let Some(run_loop) = CFRunLoop::current()
        {
            run_loop.stop();
        }
    }

    fn failed_callback(&mut self) {
        self.shared.fail();
        while let Ok(command) = self.commands.try_recv() {
            command.fail();
        }
        // Keep the settled tail for as long as the physical button is down. The one-second bound
        // starts only once HID reports the release, as in reconcile_buttons.
        let released = self.settled_primary
            && !CGEventSource::button_state(
                CGEventSourceStateID::HIDSystemState,
                CGMouseButton::Left,
            );
        if self.pending_settled_tail(Instant::now(), released) {
            // Dead callbacks never re-enable the tap; the tail still needs it.
            if let Some(tap) = &self.tap
                && !CGEvent::tap_is_enabled(tap)
            {
                CGEvent::tap_enable(tap, true);
            }
        } else if let Some(run_loop) = CFRunLoop::current() {
            run_loop.stop();
        }
    }

    fn tap_disabled(&self) {
        if let Some(tap) = &self.tap {
            CGEvent::tap_enable(tap, true);
        }
        if self.shared.capturing.load(Ordering::Acquire) {
            let _ = self.shared.finish(EndReason::Lost, None);
        }
    }

    /// Runs one pointer event through the pure edge decision and emits what it asks for.
    fn edge_input(&mut self, input: PointerInput, hits: &[(PortalId, f64)], at: MonoTime) {
        let update = edge_update(
            input,
            &mut self.held_buttons,
            |number| {
                u32::try_from(number)
                    .is_ok_and(|number| CGEventSource::button_state(SESSION, CGMouseButton(number)))
            },
            &self.pressed,
            hits,
        );
        for (portal, position) in update.press {
            self.shared.event(
                0,
                CaptureEvent::EdgePressed {
                    portal,
                    position,
                    at,
                },
            );
        }
        for portal in update.release {
            self.shared
                .event(0, CaptureEvent::EdgeReleased { portal, at });
        }
        self.pressed = update.pressed;
    }

    /// Returns true only when the OS should receive this event.
    fn event(&mut self, kind: CGEventType, event: &CGEvent) -> bool {
        if self.shared.dead.load(Ordering::Acquire) {
            return !self.swallow_settled_tail(kind, event);
        }
        if matches!(
            kind,
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
        ) {
            self.tap_disabled();
            return true;
        }
        if self.shared.capturing.load(Ordering::Acquire) && !self.shared.gate.is_open() {
            let _ = self.shared.finish(EndReason::Lost, None);
        }
        let at = event_time(event, &self.shared.time);
        let active = self.shared.active.load(Ordering::Acquire);
        let capturing = matches!(active & 3, PENDING | EFFECTIVE)
            && self.shared.capturing.load(Ordering::Acquire);
        let token = active >> 2;
        let integer = |field| CGEvent::integer_value_field(Some(event), field);
        let double = |field| CGEvent::double_value_field(Some(event), field);
        if self.monitor
            && !capturing
            && integer(CGEventField::EventSourceUserData) != INJECTED
            && integer(CGEventField::EventSourceUnixProcessID) == 0
            && integer(CGEventField::EventSourceStateID)
                == i64::from(CGEventSourceStateID::HIDSystemState.0)
            && self
                .last_activity
                .is_none_or(|last| at.saturating_duration_since(last) >= CALL_BUDGET)
        {
            self.last_activity = Some(at);
            self.shared.event(0, CaptureEvent::LocalActivity { at });
        }
        // Crosspane's own injected input (as an E1 target, or into a projected window) belongs to
        // the OS: never local motion, an edge press or a captured key. Otherwise injected motion
        // reaching an edge could cross back to the controller.
        if integer(CGEventField::EventSourceUserData) == INJECTED {
            return true;
        }
        let motion = matches!(
            kind,
            CGEventType::MouseMoved
                | CGEventType::LeftMouseDragged
                | CGEventType::RightMouseDragged
                | CGEventType::OtherMouseDragged
        );
        if motion {
            let dx = double(CGEventField::MouseEventDeltaX);
            let dy = double(CGEventField::MouseEventDeltaY);
            if capturing {
                if let Some(display) = self.display {
                    self.shared.event(
                        token,
                        CaptureEvent::Motion {
                            dx: dx * display.scale,
                            dy: dy * display.scale,
                            kind: MotionKind::Accelerated {
                                display: display.id,
                            },
                            at,
                        },
                    );
                    self.pin_motion(token, CGEvent::location(Some(event)));
                }
            } else if self.subscribed {
                self.sync_portals();
                let location = CGEvent::location(Some(event));
                let hits: Vec<_> = self
                    .portals
                    .iter()
                    .filter_map(|portal| {
                        portal_hit(*portal, location, dx, dy)
                            .map(|position| (portal.portal.id, position))
                    })
                    .collect();
                let input = if kind == CGEventType::MouseMoved {
                    PointerInput::Moved
                } else {
                    PointerInput::Dragged
                };
                self.edge_input(input, &hits, at);
                if kind == CGEventType::LeftMouseDragged && self.suppressed_buttons[0] == 0 {
                    self.drag.pointer = location;
                    if self.drag.should_lookup(true, &self.portals, location) {
                        if self.drag.lookup_due(at) {
                            match (self.window_at)(location) {
                                Ok(window) => self.drag.sample(window, location, &self.portals),
                                Err(error) => {
                                    tracing::debug!(%error, "drag window lookup unavailable");
                                    self.drag.sample(None, location, &self.portals);
                                }
                            }
                        }
                    } else {
                        for event in self.drag.clear(at) {
                            self.shared.event(0, event);
                        }
                    }
                    for event in self.drag.update(&self.portals, &hits, at) {
                        self.shared.event(0, event);
                    }
                } else {
                    for event in self.drag.clear(at) {
                        self.shared.event(0, event);
                    }
                }
            }
            let settled_tail = kind == CGEventType::LeftMouseDragged
                && self.settled_primary
                && self.suppressed_buttons[0] != 0;
            return (!capturing && !settled_tail) || self.shared.dead.load(Ordering::Acquire);
        }
        if matches!(
            kind,
            CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged
        ) {
            let Ok(keycode) = u16::try_from(integer(CGEventField::KeyboardEventKeycode)) else {
                return !capturing;
            };
            let flags = CGEvent::flags(Some(event));
            if keycode == 0x39 {
                let current = locks(flags);
                if current != self.lock_keys {
                    self.lock_keys = current;
                    if self.subscribed {
                        self.shared.event(0, CaptureEvent::LockKeys(current));
                    }
                }
                return !capturing;
            }
            let down = if kind == CGEventType::FlagsChanged {
                let Some(down) = modifier_down(keycode, flags) else {
                    return !capturing;
                };
                down
            } else {
                kind == CGEventType::KeyDown
            };
            let index = usize::from(keycode);
            if index >= self.suppressed_keys.len() {
                return !capturing;
            }
            let repeat = integer(CGEventField::KeyboardEventAutorepeat) != 0;
            if self.local_keys[index] {
                if !down {
                    self.local_keys[index] = false;
                    // The engine removes this activation-held key from its release chord;
                    // the OS also receives the up because it received the original down.
                    if capturing && let Some(usage) = macos_to_hid(keycode) {
                        self.shared
                            .event(token, CaptureEvent::Key { usage, down, at });
                    }
                    return true;
                }
                return !capturing;
            }
            if self.suppressed_keys[index] != 0 {
                if !down {
                    self.suppressed_keys[index] = 0;
                    if capturing && let Some(usage) = macos_to_hid(keycode) {
                        // Also clears a key seeded in held_keys from an earlier capture.
                        // The engine's router ignores an up with no routed down in this capture.
                        self.shared
                            .event(token, CaptureEvent::Key { usage, down, at });
                    }
                }
                return self.shared.dead.load(Ordering::Acquire);
            }
            if capturing {
                if down {
                    self.suppressed_keys[index] = token;
                }
                if !repeat && let Some(usage) = macos_to_hid(keycode) {
                    self.shared
                        .event(token, CaptureEvent::Key { usage, down, at });
                }
                return self.shared.dead.load(Ordering::Acquire);
            }
            return true;
        }
        let button_down = matches!(
            kind,
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown
        );
        let button_up = matches!(
            kind,
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp | CGEventType::OtherMouseUp
        );
        if button_down || button_up {
            if matches!(kind, CGEventType::LeftMouseDown | CGEventType::LeftMouseUp) {
                for event in self.drag.clear(at) {
                    self.shared.event(0, event);
                }
            }
            let number = match kind {
                CGEventType::LeftMouseDown | CGEventType::LeftMouseUp => 0,
                CGEventType::RightMouseDown | CGEventType::RightMouseUp => 1,
                _ => integer(CGEventField::MouseEventButtonNumber),
            };
            let Ok(index) = usize::try_from(number) else {
                return !capturing;
            };
            if index >= self.suppressed_buttons.len() {
                return !capturing;
            }
            // Track every local button, suppressed or not. A down also releases any pressed
            // portal at once; an up never presses one, so only later motion can.
            self.edge_input(
                if button_down {
                    PointerInput::ButtonDown(index)
                } else {
                    PointerInput::ButtonUp(index)
                },
                &[],
                at,
            );
            if self.suppressed_buttons[index] != 0 {
                if button_up {
                    let pressed_in = self.suppressed_buttons[index];
                    self.suppressed_buttons[index] = 0;
                    if index == 0 {
                        self.settled_primary = false;
                        self.settled_tail_until = None;
                    }
                    if capturing
                        && pressed_in == token
                        && let Some(button) = button(number)
                    {
                        self.shared.event(
                            token,
                            CaptureEvent::Button {
                                button,
                                down: false,
                                at,
                            },
                        );
                    }
                }
                return self.shared.dead.load(Ordering::Acquire);
            }
            if index == 0 && self.settled_capture == token && capturing {
                if button_up {
                    return false;
                }
                self.settled_capture = 0;
            }
            if capturing {
                if button_down {
                    self.suppressed_buttons[index] = token;
                }
                if let Some(button) = button(number) {
                    self.shared.event(
                        token,
                        CaptureEvent::Button {
                            button,
                            down: button_down,
                            at,
                        },
                    );
                }
                return self.shared.dead.load(Ordering::Acquire);
            }
            return true;
        }
        if kind == CGEventType::ScrollWheel && capturing {
            let continuous = integer(CGEventField::ScrollWheelEventIsContinuous) != 0;
            let (x, y) = if continuous {
                (
                    double(CGEventField::ScrollWheelEventPointDeltaAxis2),
                    double(CGEventField::ScrollWheelEventPointDeltaAxis1),
                )
            } else {
                (
                    integer(CGEventField::ScrollWheelEventDeltaAxis2) as f64,
                    integer(CGEventField::ScrollWheelEventDeltaAxis1) as f64,
                )
            };
            self.shared.event(
                token,
                CaptureEvent::Scroll {
                    delta: scroll(
                        continuous,
                        x,
                        y,
                        integer(CGEventField::ScrollWheelEventScrollPhase),
                        integer(CGEventField::ScrollWheelEventMomentumPhase),
                    ),
                    at,
                },
            );
        }
        !capturing || self.shared.dead.load(Ordering::Acquire)
    }
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: *mut CGEvent,
    info: *mut c_void,
) -> *mut CGEvent {
    if info.is_null() {
        return event;
    }
    // SAFETY: TapGuard owns this Box until after all callback sources are invalidated, even
    // during unwinding. Callbacks run serially on this thread and never recurse into the loop.
    let state = unsafe { &mut *info.cast::<TapState>() };
    // Record ownership before event() consumes the up, including a concurrent delivery failure.
    let owned_tail = if matches!(
        kind,
        CGEventType::LeftMouseDragged | CGEventType::LeftMouseUp
    ) {
        // SAFETY: known mouse callbacks borrow a non-null CGEvent for this invocation only.
        unsafe { event.as_ref() }.is_some_and(|input| state.owns_settled_tail(kind, input))
    } else {
        false
    };
    let result = catch_unwind(AssertUnwindSafe(|| {
        if state.shared.dead.load(Ordering::Acquire) {
            // SAFETY: callback borrows a non-null CGEvent only for this invocation; disabled
            // notifications may be null and never enter the settled-tail matcher.
            if let Some(input) = unsafe { event.as_ref() }
                && state.swallow_settled_tail(kind, input)
            {
                return ptr::null_mut();
            }
            return event;
        }
        if matches!(
            kind,
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
        ) {
            state.tap_disabled();
            return event;
        }
        // SAFETY: non-NULL events are borrowed only for this callback; NULL is never dereferenced.
        let Some(input) = (unsafe { event.as_ref() }) else {
            return event;
        };
        if state.event(kind, input) {
            event
        } else {
            ptr::null_mut()
        }
    }));
    match result {
        // event() may see delivery die during a pin and request the ordinary fail-open path.
        // A settled physical tail is still ours, including the up that already cleared its ledger.
        Ok(_) if owned_tail => ptr::null_mut(),
        Ok(result) if !state.shared.dead.load(Ordering::Acquire) => result,
        Ok(_) => event,
        Err(_) => {
            state.failed_callback();
            // A panic while handling the settled tail must not hand the seat a second up.
            if owned_tail { ptr::null_mut() } else { event }
        }
    }
}

unsafe extern "C-unwind" fn command_callback(info: *mut c_void) {
    if info.is_null() {
        return;
    }
    // SAFETY: TapGuard keeps the context alive through source invalidation, including unwinding.
    let state = unsafe { &mut *info.cast::<TapState>() };
    if catch_unwind(AssertUnwindSafe(|| state.commands())).is_err() {
        state.failed_callback();
    }
}

unsafe extern "C-unwind" fn timer_callback(_timer: *mut CFRunLoopTimer, info: *mut c_void) {
    if info.is_null() {
        return;
    }
    // SAFETY: TapGuard invalidates the timer before freeing its single-thread-owned context.
    let state = unsafe { &mut *info.cast::<TapState>() };
    if catch_unwind(AssertUnwindSafe(|| state.poll())).is_err() {
        state.failed_callback();
    }
}

struct TapGuard {
    state: *mut TapState,
    run_loop: Option<CFRetained<CFRunLoop>>,
    tap_source: Option<CFRetained<CFRunLoopSource>>,
    command_source: Option<CFRetained<CFRunLoopSource>>,
    timer: Option<CFRetained<CFRunLoopTimer>>,
}

impl Drop for TapGuard {
    fn drop(&mut self) {
        // SAFETY: unique ownership came from Box::into_raw; callbacks have returned before
        // run_tap can exit/unwind. The allocation is freed only after every source is invalidated.
        let state = unsafe { &mut *self.state };
        if state.shared.stop.load(Ordering::Acquire) {
            let _ = state.shared.finish(EndReason::Lost, None);
        } else {
            state.shared.fail();
        }
        if let Some(tap) = &state.tap {
            CGEvent::tap_enable(tap, false);
            tap.invalidate();
        }
        // SAFETY: immutable process-lifetime CoreFoundation mode constant.
        let mode = unsafe { kCFRunLoopDefaultMode };
        if let Some(timer) = &self.timer {
            timer.invalidate();
            if let Some(run_loop) = &self.run_loop {
                run_loop.remove_timer(Some(timer), mode);
            }
        }
        for source in [&self.command_source, &self.tap_source]
            .into_iter()
            .flatten()
        {
            source.invalidate();
            if let Some(run_loop) = &self.run_loop {
                run_loop.remove_source(Some(source), mode);
            }
        }
        while let Ok(command) = state.commands.try_recv() {
            command.fail();
        }
        // SAFETY: all native callbacks are now disabled/invalidated, including partial setup and
        // unwinding exits; this is the only reconstruction of the Box::into_raw allocation.
        drop(unsafe { Box::from_raw(self.state) });
    }
}

fn run_tap(
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    ready: Sender<Result<Arc<Wake>, PlatformError>>,
) {
    let state = Box::into_raw(Box::new(TapState {
        shared: shared.clone(),
        commands,
        tap: None,
        portal_config: Arc::new(Vec::new()),
        portals: Arc::new(Vec::new()),
        pressed: HashSet::new(),
        subscribed: false,
        monitor: false,
        last_activity: None,
        last_secure_poll: Instant::now(),
        last_geometry_poll: Instant::now(),
        blinded: secure_input(),
        lock_keys: locks(CGEventSource::flags_state(SESSION)),
        display: None,
        pin: None,
        local_keys: [false; 128],
        suppressed_keys: [0; 128],
        suppressed_buttons: [0; 256],
        held_buttons: HeldButtons::default(),
        drag: drag::Move::default(),
        window_at: Box::new(drag::under_pointer),
        tiling_busy: Arc::new(AtomicBool::new(false)),
        settled_primary: false,
        settled_capture: 0,
        settled_tail_until: None,
    }));
    let mut guard = TapGuard {
        state,
        run_loop: None,
        tap_source: None,
        command_source: None,
        timer: None,
    };
    let info = state.cast::<c_void>();
    let mut setup = || -> Result<_, PlatformError> {
        if shared.stop.load(Ordering::Acquire) {
            return Err(PlatformError::Timeout);
        }
        check_permissions()?;
        let mask = [
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::ScrollWheel,
        ]
        .iter()
        .fold(0u64, |mask, kind| mask | 1 << kind.0);
        // SAFETY: public suppressing session tap, with a NULL-aware C callback. TapGuard owns
        // the Box::into_raw context and invalidates the tap before freeing it on every exit.
        let tap = unsafe {
            CGEventTapCreate(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mask,
                tap_callback,
                info,
            )
        };
        let tap = NonNull::new(tap).ok_or_else(|| {
            check_permissions()
                .err()
                .unwrap_or_else(|| PlatformError::Backend("CGEventTapCreate failed".into()))
        })?;
        // SAFETY: CGEventTapCreate returns an owned +1 reference on success.
        let tap = unsafe { CFRetained::from_raw(tap) };
        // SAFETY: setup runs before callbacks are added to the loop; the guard owns this state.
        unsafe { (*state).tap = Some(tap.clone()) };
        let run_loop = CFRunLoop::current()
            .ok_or_else(|| PlatformError::Backend("no capture run loop".into()))?;
        guard.run_loop = Some(run_loop.clone());
        let tap_source = CFMachPort::new_run_loop_source(None, Some(&tap), 0)
            .ok_or_else(|| PlatformError::Backend("create tap run-loop source".into()))?;
        guard.tap_source = Some(tap_source.clone());
        let mut context = CFRunLoopSourceContext {
            version: 0,
            info,
            retain: None,
            release: None,
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: Some(command_callback),
        };
        // SAFETY: default allocator and version-zero context; callbacks borrow the same stable box.
        let command_source = unsafe { CFRunLoopSource::new(None, -1, &mut context) }
            .ok_or_else(|| PlatformError::Backend("create capture command source".into()))?;
        guard.command_source = Some(command_source.clone());
        let mut context = CFRunLoopTimerContext {
            version: 0,
            info,
            retain: None,
            release: None,
            copyDescription: None,
        };
        // SAFETY: default allocator and valid context; timer runs on the tap thread only.
        let timer = unsafe {
            CFRunLoopTimer::new(
                None,
                CFAbsoluteTimeGetCurrent() + 0.05,
                0.05,
                0,
                0,
                Some(timer_callback),
                &mut context,
            )
        }
        .ok_or_else(|| PlatformError::Backend("create Secure Input timer".into()))?;
        guard.timer = Some(timer.clone());
        // SAFETY: the valid thread-owned timer accepts a nonnegative tolerance.
        unsafe { timer.set_tolerance(0.0) };
        Ok((run_loop, tap_source, command_source, timer))
    };
    let (run_loop, tap_source, command_source, timer) = match setup() {
        Ok(resources) => resources,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    // SAFETY: immutable process-lifetime CoreFoundation run-loop mode constant.
    let mode = unsafe { kCFRunLoopDefaultMode };
    run_loop.add_source(Some(&tap_source), mode);
    run_loop.add_source(Some(&command_source), mode);
    run_loop.add_timer(Some(&timer), mode);
    let wake = Arc::new(Wake {
        run_loop: run_loop.clone(),
        source: command_source.clone(),
    });
    if !shared.stop.load(Ordering::Acquire) && ready.send(Ok(wake)).is_ok() {
        CFRunLoop::run();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use objc2_core_foundation::CGSize;

    #[derive(Clone, Debug, PartialEq)]
    enum CursorCall {
        Associate(bool),
        Hide,
        Show,
        Warp(CGPoint),
    }

    #[derive(Default)]
    struct FakeCursor {
        calls: Mutex<Vec<(CursorCall, u64)>>,
        fail_hide: AtomicBool,
        owner: Mutex<std::sync::Weak<Shared>>,
        position: Mutex<Option<CGPoint>>,
        pause: Mutex<Option<(Sender<()>, Receiver<()>)>>,
        pause_show: Mutex<Option<(Sender<()>, Receiver<()>)>>,
        fail_warps: AtomicU64,
    }

    impl FakeCursor {
        fn record(&self, call: CursorCall) {
            let pin_token = self
                .owner
                .lock()
                .unwrap()
                .upgrade()
                .map_or(0, |shared| shared.pin_token.load(Ordering::Acquire));
            self.calls.lock().unwrap().push((call, pin_token));
        }

        fn operations(self: &Arc<Self>) -> CursorOps {
            let associate = self.clone();
            let hide = self.clone();
            let show = self.clone();
            let warp = self.clone();
            CursorOps {
                associate: Box::new(move |associated| {
                    associate.record(CursorCall::Associate(associated));
                    Ok(())
                }),
                hide: Box::new(move || {
                    hide.record(CursorCall::Hide);
                    if hide.fail_hide.load(Ordering::Acquire) {
                        return Err(PlatformError::Backend("fake hide refused".into()));
                    }
                    Ok(())
                }),
                show: Box::new(move || {
                    show.record(CursorCall::Show);
                    let pause = show.pause_show.lock().unwrap().take();
                    if let Some((entered, resume)) = pause {
                        entered.send(()).unwrap();
                        let _ = resume.recv();
                    }
                    Ok(())
                }),
                warp: Box::new(move |point| {
                    warp.record(CursorCall::Warp(point));
                    let pause = warp.pause.lock().unwrap().take();
                    if let Some((entered, resume)) = pause {
                        entered.send(()).unwrap();
                        // Dropping the test's sender also drains the fake on an assertion failure.
                        let _ = resume.recv();
                    }
                    if warp
                        .fail_warps
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                            count.checked_sub(1)
                        })
                        .is_ok()
                    {
                        return Err(PlatformError::Backend("synthetic warp failure".into()));
                    }
                    *warp.position.lock().unwrap() = Some(point);
                    Ok(())
                }),
                display: Box::new(|id| {
                    Ok(Display {
                        id,
                        bounds: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(100.0, 100.0)),
                        scale: 2.0,
                    })
                }),
            }
        }

        fn warps(&self) -> Vec<CGPoint> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(call, _)| match call {
                    CursorCall::Warp(point) => Some(*point),
                    _ => None,
                })
                .collect()
        }

        fn pause_next_warp(&self) -> (Receiver<()>, Sender<()>) {
            let (entered, paused) = mpsc::channel();
            let (resume, resumed) = mpsc::channel();
            *self.pause.lock().unwrap() = Some((entered, resumed));
            (paused, resume)
        }

        fn pause_next_show(&self) -> (Receiver<()>, Sender<()>) {
            let (entered, paused) = mpsc::channel();
            let (resume, resumed) = mpsc::channel();
            *self.pause_show.lock().unwrap() = Some((entered, resumed));
            (paused, resume)
        }
    }

    // Every fixture uses fake cursor operations, even when testing acquired-resource recovery.
    fn shared_fixture() -> (Arc<Shared>, Receiver<Delivery>) {
        let (shared, receiver, _) = shared_cursor_fixture(true);
        (shared, receiver)
    }

    fn shared_cursor_fixture(
        pin_enabled: bool,
    ) -> (Arc<Shared>, Receiver<Delivery>, Arc<FakeCursor>) {
        let gate = IoGate::new();
        gate.set_engine_permits(true);
        gate.set_session_permits(true);
        let (output, receiver) = mpsc::sync_channel(DELIVERY_LIMIT);
        let cursor = Arc::new(FakeCursor::default());
        let shared = Arc::new(Shared {
            gate,
            output,
            active: AtomicU64::new(0),
            capturing: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            cursor: cursor.operations(),
            pin_enabled,
            pin_token: AtomicU64::new(0),
            pin_metrics: PinMetrics::default(),
            pin_inflight: AtomicU64::new(0),
            pin_finish: AtomicU64::new(0),
            release_x: AtomicU64::new(0),
            release_y: AtomicU64::new(0),
            detached: AtomicBool::new(false),
            hidden: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            time: AtomicU8::new(UNKNOWN_TIME),
            portals: Mutex::new(Arc::new(Vec::new())),
        });
        *cursor.owner.lock().unwrap() = Arc::downgrade(&shared);
        (shared, receiver, cursor)
    }

    #[test]
    fn finish_active_state_machine() {
        for phase in [PENDING, EFFECTIVE] {
            let (shared, receiver) = shared_fixture();
            shared.active.store(7 << 2 | phase, Ordering::Release);
            shared.capturing.store(true, Ordering::Release);
            let request = Request {
                deadline: Instant::now() + CALL_BUDGET,
                cancelled: AtomicBool::new(false),
                epoch: 0,
            };
            // A late failed begin must not end another generation.
            shared.finish_token(EndReason::Lost, 6).unwrap();
            assert_eq!(shared.active.load(Ordering::Acquire), 7 << 2 | phase);
            shared.finish(EndReason::Requested, None).unwrap();
            assert_eq!(shared.active.load(Ordering::Acquire), 0);
            assert!(!shared.capturing.load(Ordering::Acquire));
            assert!(!request.valid(&shared));
            if phase == EFFECTIVE {
                assert!(matches!(
                    receiver.try_recv(),
                    Ok(Delivery::End(7, EndReason::Requested))
                ));
            }
            // Idle end ignores even an invalid warp, and repeated abort has no second Ended.
            shared
                .finish(
                    EndReason::Requested,
                    Some((DisplayId(0), PointDevice::new(f64::NAN, 0.0))),
                )
                .unwrap();
            shared.finish(EndReason::Aborted, None).unwrap();
            assert!(receiver.try_recv().is_err());
        }
        let (shared, receiver) = shared_fixture();
        shared.active.store(8 << 2 | RECOVERING, Ordering::Release);
        shared.capturing.store(true, Ordering::Release);
        shared.finish(EndReason::Aborted, None).unwrap();
        assert!(!shared.capturing.load(Ordering::Acquire));
        assert!(matches!(
            receiver.try_recv(),
            Ok(Delivery::End(8, EndReason::Aborted))
        ));
        // The original recovery still owns this generation; abort never waits for it.
        assert_eq!(shared.active.load(Ordering::Acquire), 8 << 2 | RECOVERING);
    }

    #[test]
    fn failed_cursor_restore_is_retried() {
        let flag = AtomicBool::new(true);
        assert!(
            restore_flag(&flag, || Err(PlatformError::Backend(
                "synthetic failure".into()
            )))
            .is_err()
        );
        assert!(flag.load(Ordering::Acquire));
        restore_flag(&flag, || Ok(())).unwrap();
        assert!(!flag.load(Ordering::Acquire));
        restore_flag(&flag, || panic!("successful restore must not repeat")).unwrap();
    }

    #[test]
    fn delivery_exit_recovers_on_sink_panic_and_unwind() {
        let (shared, receiver) = shared_fixture();
        shared.active.store(1 << 2 | EFFECTIVE, Ordering::Release);
        shared.capturing.store(true, Ordering::Release);
        shared
            .queue(Delivery::Subscribe(
                Arc::new(|_| panic!("synthetic sink panic")),
                LockKeys::default(),
                false,
            ))
            .unwrap();
        let worker_shared = shared.clone();
        std::thread::spawn(move || deliver(worker_shared, receiver))
            .join()
            .unwrap();
        assert!(shared.dead.load(Ordering::Acquire));
        assert!(!shared.capturing.load(Ordering::Acquire));
        assert_eq!(shared.active.load(Ordering::Acquire), 0);
        assert!(shared.available().is_err());

        let (shared, _receiver) = shared_fixture();
        shared.active.store(2 << 2 | EFFECTIVE, Ordering::Release);
        shared.capturing.store(true, Ordering::Release);
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _guard = DeliveryGuard(shared.clone());
                panic!("synthetic worker unwind");
            }))
            .is_err()
        );
        assert!(shared.dead.load(Ordering::Acquire));
        assert!(!shared.capturing.load(Ordering::Acquire));
        assert_eq!(shared.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn delivery_overflow_ends_and_drops_backlog() {
        let (shared, receiver) = shared_fixture();
        let (events, observed) = mpsc::channel();
        let (started, activation) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(blocked);
        shared
            .queue(Delivery::Subscribe(
                Arc::new(move |event| {
                    let is_start = matches!(event, CaptureEvent::Started { .. });
                    events.send(event).unwrap();
                    if is_start {
                        started.send(()).unwrap();
                        blocked
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(2))
                            .unwrap();
                    }
                }),
                LockKeys::default(),
                false,
            ))
            .unwrap();
        shared.active.store(1 << 2 | PENDING, Ordering::Release);
        shared.capturing.store(true, Ordering::Release);
        let (reply, result) = mpsc::channel();
        shared
            .queue(Delivery::Activate {
                token: 1,
                id: CaptureId(1),
                start: CaptureStart {
                    held_keys: Vec::new(),
                    lock_keys: LockKeys::default(),
                },
                request: Arc::new(Request {
                    deadline: Instant::now() + Duration::from_secs(2),
                    cancelled: AtomicBool::new(false),
                    epoch: 0,
                }),
                reply,
            })
            .unwrap();
        let worker_shared = shared.clone();
        let worker = std::thread::spawn(move || deliver(worker_shared, receiver));
        activation.recv_timeout(Duration::from_secs(1)).unwrap();
        for _ in 0..DELIVERY_LIMIT {
            shared
                .queue(Delivery::Event(
                    1,
                    CaptureEvent::LockKeys(LockKeys::default()),
                ))
                .unwrap();
        }
        assert!(
            shared
                .queue(Delivery::Event(
                    1,
                    CaptureEvent::LockKeys(LockKeys::default())
                ))
                .is_err()
        );
        assert!(!shared.capturing.load(Ordering::Acquire));
        assert_eq!(shared.active.load(Ordering::Acquire), 0);
        assert!(shared.available().is_err());
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(result.recv().unwrap().is_err());
        let events: Vec<_> = observed.try_iter().collect();
        assert_eq!(events.len(), 4);
        assert!(matches!(
            events[2],
            CaptureEvent::Started { id: CaptureId(1) }
        ));
        assert!(matches!(
            events[3],
            CaptureEvent::Ended {
                id: CaptureId(1),
                reason: EndReason::Lost
            }
        ));
    }

    #[test]
    fn portals_all_edges_at_scale_two() {
        let display = Display {
            id: DisplayId(1),
            bounds: CGRect::new(CGPoint::new(10.0, 20.0), CGSize::new(100.0, 100.0)),
            scale: 2.0,
        };
        for (edge, point, dx, dy) in [
            (Edge::Left, CGPoint::new(10.0, 70.0), -1.0, 0.0),
            (Edge::Right, CGPoint::new(110.0, 70.0), 1.0, 0.0),
            (Edge::Top, CGPoint::new(60.0, 20.0), 0.0, -1.0),
            (Edge::Bottom, CGPoint::new(60.0, 120.0), 0.0, 1.0),
        ] {
            let portal = Portal {
                display,
                portal: CapturePortal {
                    id: PortalId(1),
                    display: display.id,
                    edge,
                    from: 40.0,
                    to: 160.0,
                },
            };
            assert_eq!(portal_hit(portal, point, dx, dy), Some(0.5));
            assert_eq!(portal_hit(portal, point, -dx, -dy), None);
            assert_eq!(portal_hit(portal, point, 0.0, 0.0), None);
            assert_eq!(portal_hit(portal, CGPoint::new(60.0, 70.0), dx, dy), None);
            let outside = match edge {
                Edge::Left | Edge::Right => CGPoint::new(point.x, 21.0),
                Edge::Top | Edge::Bottom => CGPoint::new(11.0, point.y),
            };
            assert_eq!(portal_hit(portal, outside, dx, dy), None);
        }
    }

    const BUTTONS: [usize; 6] = [0, 1, 2, 3, 4, 7];

    fn ids(values: &[u32]) -> HashSet<PortalId> {
        values.iter().copied().map(PortalId).collect()
    }

    fn hit(id: u32) -> (PortalId, f64) {
        (PortalId(id), 0.5)
    }

    /// Fails the test if the decision asks the OS anything.
    fn no_query(number: usize) -> bool {
        panic!("button_state queried for button {number} on a path that must not query");
    }

    #[test]
    fn held_buttons_cover_every_number() {
        let mut held = HeldButtons::default();
        assert!(!held.any());
        for number in [0, 1, 2, 3, 4, 7, 63, 64, 127, 128, 255] {
            held.set(number, true);
            assert!(held.get(number));
            assert!(held.any());
            held.set(number, false);
            assert!(!held.get(number));
            assert!(!held.any());
        }
        held.set(256, true);
        held.set(usize::MAX, true);
        assert!(!held.any());
        assert!(!held.get(256));
    }

    #[test]
    fn drag_across_a_portal_presses_nothing() {
        for button in BUTTONS {
            let mut held = HeldButtons::default();
            let mut pressed = HashSet::new();
            let down = edge_update(
                PointerInput::ButtonDown(button),
                &mut held,
                no_query,
                &pressed,
                &[],
            );
            assert_eq!(down, EdgeUpdate::default());
            // Repeated dragged motion across the stretch, as in the 2026-10-01 resize drag.
            for _ in 0..5 {
                let update = edge_update(
                    PointerInput::Dragged,
                    &mut held,
                    no_query,
                    &pressed,
                    &[hit(1), hit(2)],
                );
                assert_eq!(update, EdgeUpdate::default(), "button {button}");
                pressed = update.pressed;
            }
            assert!(held.get(button));
        }
    }

    #[test]
    fn a_drag_is_held_even_when_the_tap_missed_the_down() {
        // Tap re-enabled mid-drag: no down was seen, and button_state may already read "up".
        let mut held = HeldButtons::default();
        let update = edge_update(
            PointerInput::Dragged,
            &mut held,
            |_| false,
            &HashSet::new(),
            &[hit(1)],
        );
        assert_eq!(update, EdgeUpdate::default());
    }

    #[test]
    fn button_down_while_pressing_releases_at_once() {
        for button in BUTTONS {
            let mut held = HeldButtons::default();
            let pressed = ids(&[3, 1]);
            let update = edge_update(
                PointerInput::ButtonDown(button),
                &mut held,
                no_query,
                &pressed,
                &[hit(1), hit(3)],
            );
            assert_eq!(update.release, vec![PortalId(1), PortalId(3)]);
            assert!(update.press.is_empty());
            assert!(update.pressed.is_empty());
            assert!(held.get(button));
        }
    }

    #[test]
    fn button_up_at_the_edge_never_presses_and_later_motion_does() {
        for button in BUTTONS {
            let mut held = HeldButtons::default();
            edge_update(
                PointerInput::ButtonDown(button),
                &mut held,
                no_query,
                &HashSet::new(),
                &[],
            );
            let up = edge_update(
                PointerInput::ButtonUp(button),
                &mut held,
                no_query,
                &HashSet::new(),
                &[hit(1)],
            );
            assert_eq!(up, EdgeUpdate::default(), "button {button}");
            assert!(!held.any());
            let moved = edge_update(
                PointerInput::Moved,
                &mut held,
                no_query,
                &up.pressed,
                &[hit(1)],
            );
            assert_eq!(moved.press, vec![hit(1)]);
            assert!(moved.release.is_empty());
            assert_eq!(moved.pressed, ids(&[1]));
        }
    }

    #[test]
    fn the_last_up_decides_not_the_first() {
        let mut held = HeldButtons::default();
        let none = HashSet::new();
        for button in [0, 1] {
            edge_update(
                PointerInput::ButtonDown(button),
                &mut held,
                no_query,
                &none,
                &[],
            );
        }
        edge_update(PointerInput::ButtonUp(0), &mut held, no_query, &none, &[]);
        // Right is still down in the OS too, so a Moved (synthetic tools can send one) stays quiet.
        let update = edge_update(
            PointerInput::Moved,
            &mut held,
            |number| number == 1,
            &none,
            &[hit(1)],
        );
        assert_eq!(update, EdgeUpdate::default());
        edge_update(PointerInput::ButtonUp(1), &mut held, no_query, &none, &[]);
        let update = edge_update(PointerInput::Moved, &mut held, no_query, &none, &[hit(1)]);
        assert_eq!(update.press, vec![hit(1)]);
    }

    #[test]
    fn a_missed_up_is_reconciled_from_button_state() {
        for button in BUTTONS {
            let mut held = HeldButtons::default();
            let none = HashSet::new();
            edge_update(
                PointerInput::ButtonDown(button),
                &mut held,
                no_query,
                &none,
                &[],
            );
            // The up never reached the tap. button_state says the button is held: no press.
            let queried = std::cell::RefCell::new(Vec::new());
            let still_down = edge_update(
                PointerInput::Moved,
                &mut held,
                |number| {
                    queried.borrow_mut().push(number);
                    true
                },
                &none,
                &[hit(1)],
            );
            assert_eq!(still_down, EdgeUpdate::default());
            assert!(held.get(button));
            // button_state says it is up: the stale entry is dropped and motion presses.
            queried.borrow_mut().clear();
            let reconciled = edge_update(
                PointerInput::Moved,
                &mut held,
                |number| {
                    queried.borrow_mut().push(number);
                    false
                },
                &none,
                &[hit(1)],
            );
            assert_eq!(*queried.borrow(), vec![button], "only the tracked button");
            assert!(!held.any());
            assert_eq!(reconciled.press, vec![hit(1)]);
            assert_eq!(reconciled.pressed, ids(&[1]));
            // Reconciled for good: the next motion needs no query.
            let next = edge_update(
                PointerInput::Moved,
                &mut held,
                no_query,
                &reconciled.pressed,
                &[hit(1)],
            );
            assert_eq!(next.press, vec![hit(1)]);
        }
    }

    #[test]
    fn a_missed_up_still_held_in_the_os_releases_a_pressed_portal() {
        let mut held = HeldButtons::default();
        held.set(0, true);
        let update = edge_update(
            PointerInput::Moved,
            &mut held,
            |_| true,
            &ids(&[1]),
            &[hit(1)],
        );
        assert_eq!(update.release, vec![PortalId(1)]);
        assert!(update.press.is_empty());
        assert!(update.pressed.is_empty());
    }

    #[test]
    fn plain_motion_keeps_the_press_and_release_behaviour() {
        let mut held = HeldButtons::default();
        // Entering, staying on and leaving a stretch; no button, so no native query.
        let enter = edge_update(
            PointerInput::Moved,
            &mut held,
            no_query,
            &HashSet::new(),
            &[hit(1)],
        );
        assert_eq!(enter.press, vec![hit(1)]);
        assert!(enter.release.is_empty());
        let stay = edge_update(
            PointerInput::Moved,
            &mut held,
            no_query,
            &enter.pressed,
            &[hit(1)],
        );
        assert_eq!(stay.press, vec![hit(1)]);
        assert!(stay.release.is_empty());
        let swap = edge_update(
            PointerInput::Moved,
            &mut held,
            no_query,
            &stay.pressed,
            &[hit(2)],
        );
        assert_eq!(swap.press, vec![hit(2)]);
        assert_eq!(swap.release, vec![PortalId(1)]);
        let leave = edge_update(PointerInput::Moved, &mut held, no_query, &swap.pressed, &[]);
        assert!(leave.press.is_empty());
        assert_eq!(leave.release, vec![PortalId(2)]);
        assert!(leave.pressed.is_empty());
    }

    // The tap's own state machine, fed synthetic CGEvents. Creating an event needs no TCC grant
    // and nothing is posted; every path below avoids button_state, so the result does not
    // depend on the session's real mouse.
    fn tap_fixture() -> (TapState, Receiver<Delivery>) {
        let (tap, receiver, _) = tap_cursor_fixture(true);
        (tap, receiver)
    }

    #[test]
    fn settled_primary_tail_is_swallowed_after_early_end() {
        let (mut tap, receiver, _) = tap_cursor_fixture(true);
        let request = drag_request(&tap);
        tap.activate_begin(1, &request, drag_prepared(), Some(|| {}))
            .unwrap();
        tap.shared.finish(EndReason::Requested, None).unwrap();
        assert!(!tap.event(
            CGEventType::LeftMouseDragged,
            &pointer(CGEventType::LeftMouseDragged, 0, false)
        ));
        assert!(!tap.event(
            CGEventType::LeftMouseUp,
            &pointer(CGEventType::LeftMouseUp, 0, false)
        ));
        assert!(receiver.try_iter().all(|message| !matches!(
            message,
            Delivery::Event(_, CaptureEvent::Button { .. } | CaptureEvent::Motion { .. })
        )));
    }

    fn drag_request(tap: &TapState) -> Request {
        Request {
            deadline: Instant::now() + Duration::from_secs(1),
            cancelled: AtomicBool::new(false),
            epoch: tap.shared.epoch.load(Ordering::Acquire),
        }
    }

    fn drag_prepared() -> PreparedBegin {
        PreparedBegin {
            tap_ready: true,
            tap: None,
            display: Display {
                id: DisplayId(1),
                bounds: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(100.0, 100.0)),
                scale: 2.0,
            },
            location: CGPoint::new(99.0, 40.0),
            held_keys: vec![],
            locks: LockKeys::default(),
        }
    }

    #[test]
    fn drag_refusal_requires_detected_portal_and_primary_only_without_actions() {
        let (mut tap, _, cursor) = tap_cursor_fixture(true);
        let mut held = HeldButtons::default();
        held.set(0, true);
        assert!(matches!(
            drag_allowed(MouseButton(2), held, true),
            Err(PlatformError::Unsupported("begin_drag button"))
        ));
        assert!(matches!(
            drag_allowed(MouseButton::PRIMARY, held, false),
            Err(PlatformError::PointerButtonHeld)
        ));
        held.set(1, true);
        assert!(matches!(
            drag_allowed(MouseButton::PRIMARY, held, true),
            Err(PlatformError::PointerButtonHeld)
        ));
        assert!(cursor.calls.lock().unwrap().is_empty());
        let posted = std::cell::Cell::new(0);
        let mut prepared = drag_prepared();
        prepared.tap_ready = false;
        assert!(
            tap.activate_begin(
                1,
                &drag_request(&tap),
                prepared,
                Some(|| posted.set(posted.get() + 1))
            )
            .is_err()
        );
        assert_eq!(posted.get(), 0);
        assert!(cursor.calls.lock().unwrap().is_empty());
        tap.shared.gate.set_engine_permits(false);
        assert!(matches!(
            tap.activate_begin(
                1,
                &drag_request(&tap),
                drag_prepared(),
                Some(|| posted.set(1))
            ),
            Err(PlatformError::Locked)
        ));
        assert_eq!(posted.get(), 0);
        assert!(cursor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn drag_cursor_setup_failure_rolls_back_before_any_up() {
        let (mut tap, _, cursor) = tap_cursor_fixture(true);
        cursor.fail_hide.store(true, Ordering::Release);
        let posted = std::cell::Cell::new(0);
        let request = Arc::new(drag_request(&tap));
        let (reply, answer) = mpsc::channel();
        tap.begin_with(CaptureId(1), 1, request, reply, |tap, request| {
            tap.activate_begin(1, request, drag_prepared(), Some(|| posted.set(1)))
        });
        assert!(answer.recv().unwrap().is_err());
        assert_eq!(posted.get(), 0);
        assert_eq!(tap.suppressed_buttons[0], 0);
        assert_eq!(tap.shared.active.load(Ordering::Acquire), 0);
        assert!(!tap.shared.detached.load(Ordering::Acquire));
        assert!(!tap.shared.hidden.load(Ordering::Acquire));
    }

    #[test]
    fn drag_settlement_follows_installed_tap_and_cursor_setup_and_delivers_one_up() {
        let (mut tap, receiver, cursor) = tap_cursor_fixture(true);
        let posted = std::cell::Cell::new(0);
        let request = drag_request(&tap);
        let start = tap
            .activate_begin(
                1,
                &request,
                drag_prepared(),
                Some(|| {
                    assert_eq!(
                        cursor
                            .calls
                            .lock()
                            .unwrap()
                            .iter()
                            .map(|(call, _)| call.clone())
                            .collect::<Vec<_>>(),
                        [CursorCall::Associate(false), CursorCall::Hide]
                    );
                    posted.set(posted.get() + 1);
                }),
            )
            .unwrap();
        assert_eq!(posted.get(), 1);
        assert!(start.held_keys.is_empty());
        assert!(tap.settled_primary);
        assert!(!tap.event(
            CGEventType::LeftMouseUp,
            &pointer(CGEventType::LeftMouseUp, 0, false)
        ));
        // A duplicate physical up is swallowed in capture, but never emitted twice.
        assert!(!tap.event(
            CGEventType::LeftMouseUp,
            &pointer(CGEventType::LeftMouseUp, 0, false)
        ));
        let ups = receiver
            .try_iter()
            .filter(|message| {
                matches!(
                    message,
                    Delivery::Event(
                        1,
                        CaptureEvent::Button {
                            button: MouseButton::PRIMARY,
                            down: false,
                            ..
                        }
                    )
                )
            })
            .count();
        assert_eq!(ups, 1);
    }

    #[test]
    fn drag_begin_sequence_delivers_start_one_up_and_end_without_late_events() {
        let (mut tap, receiver, _) = tap_cursor_fixture(true);
        let shared = tap.shared.clone();
        let events = Arc::new(Mutex::new(Vec::new()));
        let out = events.clone();
        shared
            .queue(Delivery::Subscribe(
                Arc::new(move |event| out.lock().unwrap().push(event)),
                LockKeys::default(),
                false,
            ))
            .unwrap();
        let worker = std::thread::spawn(move || deliver(shared, receiver));
        let posted = std::cell::Cell::new(0);
        let request = Arc::new(drag_request(&tap));
        let (reply, answer) = mpsc::channel();
        tap.begin_with(CaptureId(1), 1, request, reply, |tap, request| {
            tap.activate_begin(
                1,
                request,
                drag_prepared(),
                Some(|| posted.set(posted.get() + 1)),
            )
        });
        answer
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        for _ in 0..2 {
            assert!(!tap.event(
                CGEventType::LeftMouseUp,
                &pointer(CGEventType::LeftMouseUp, 0, false)
            ));
        }
        tap.shared.finish(EndReason::Requested, None).unwrap();
        // Model an event already queued by the tap while end was taking ownership. The delivery
        // generation check must drop it after Ended, even though the queue accepts the message.
        tap.shared.event(
            1,
            CaptureEvent::Button {
                button: MouseButton::PRIMARY,
                down: false,
                at: clock::now(),
            },
        );
        tap.shared.queue(Delivery::Stop).unwrap();
        worker.join().unwrap();
        assert_eq!(posted.get(), 1);
        let events = events.lock().unwrap();
        let sequence: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    CaptureEvent::Started { .. }
                        | CaptureEvent::Button { .. }
                        | CaptureEvent::Ended { .. }
                )
            })
            .collect();
        assert!(matches!(
            sequence.as_slice(),
            [
                CaptureEvent::Started { id: CaptureId(1) },
                CaptureEvent::Button {
                    button: MouseButton::PRIMARY,
                    down: false,
                    ..
                },
                CaptureEvent::Ended {
                    id: CaptureId(1),
                    reason: EndReason::Requested
                }
            ]
        ));
    }

    #[test]
    fn detected_pressed_drag_begin_does_not_release_consumed_edge_before_started() {
        let (mut tap, receiver, _) = tap_cursor_fixture(true);
        for x in [40.0, 45.0, 99.0] {
            tap.drag.sample(
                Some(drag::WindowFact {
                    window: crosspane_types::id::WindowId(42),
                    pid: 7,
                    scale: 1.0,
                    frame: crosspane_types::geom::RectLogical::new(
                        crosspane_types::geom::PointLogical::new(x - 80.0, 38.0),
                        crosspane_types::geom::SizeLogical::new(400.0, 300.0),
                    ),
                }),
                CGPoint::new(x, 50.0),
                &tap.portals,
            );
        }
        for event in tap
            .drag
            .update(&tap.portals, &[(PortalId(1), 50.0)], clock::now())
        {
            tap.shared.event(0, event);
        }
        assert!(tap.drag.at_edge(PortalId(1)).is_some());
        let events = Arc::new(Mutex::new(Vec::new()));
        let out = events.clone();
        tap.shared
            .queue(Delivery::Subscribe(
                Arc::new(move |event| out.lock().unwrap().push(event)),
                LockKeys::default(),
                false,
            ))
            .unwrap();
        let request = Arc::new(drag_request(&tap));
        let (reply, answer) = mpsc::channel();
        tap.begin_with(CaptureId(1), 1, request, reply, |tap, request| {
            tap.activate_begin(1, request, drag_prepared(), Some(|| {}))
        });
        tap.shared.queue(Delivery::Stop).unwrap();
        deliver(tap.shared.clone(), receiver);
        answer.recv().unwrap().unwrap();
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, CaptureEvent::Started { .. }))
        );
        assert!(events.iter().all(|event| !matches!(
            event,
            CaptureEvent::EdgeReleased {
                portal: PortalId(1),
                ..
            }
        )));
        assert!(tap.drag.at_edge(PortalId(1)).is_none());
    }

    #[test]
    fn failed_activation_enqueue_keeps_settled_tail_in_native_callback() {
        for failure in 0..3 {
            let (mut tap, receiver, _) = tap_cursor_fixture(true);
            let mut receiver = Some(receiver);
            match failure {
                0 => drop(receiver.take()),
                1 => {
                    for _ in 0..DELIVERY_LIMIT {
                        tap.shared
                            .output
                            .try_send(Delivery::Event(
                                0,
                                CaptureEvent::LockKeys(LockKeys::default()),
                            ))
                            .unwrap();
                    }
                }
                _ => tap
                    .shared
                    .queue(Delivery::Subscribe(
                        Arc::new(|event| {
                            if matches!(event, CaptureEvent::Started { .. }) {
                                panic!("fake sink refused Started");
                            }
                        }),
                        LockKeys::default(),
                        false,
                    ))
                    .unwrap(),
            }
            let posted = std::cell::Cell::new(0);
            let request = Arc::new(drag_request(&tap));
            let (reply, answer) = mpsc::channel();
            tap.begin_with(CaptureId(1), 1, request, reply, |tap, request| {
                tap.activate_begin(1, request, drag_prepared(), Some(|| posted.set(1)))
            });
            if failure == 2 {
                deliver(tap.shared.clone(), receiver.take().unwrap());
            }
            assert!(!answer.recv().is_ok_and(|reply| reply.is_ok()));
            assert_eq!(posted.get(), 1);
            assert!(tap.shared.dead.load(Ordering::Acquire));
            assert!(tap.pending_settled_tail(Instant::now(), true));
            for kind in [CGEventType::LeftMouseDragged, CGEventType::LeftMouseUp] {
                let event = pointer(kind, 0, false);
                // SAFETY: fake tap state remains alive; CGEvent is borrowed, never posted, and no
                // native tap exists. This invokes the production callback's terminal branch only.
                let result = unsafe {
                    tap_callback(
                        ptr::null_mut(),
                        kind,
                        (&*event as *const CGEvent).cast_mut(),
                        (&mut tap as *mut TapState).cast(),
                    )
                };
                assert!(
                    result.is_null(),
                    "settled physical tail passed after delivery failed"
                );
            }
            assert_eq!(tap.suppressed_buttons[0], 0);
            assert!(!tap.pending_settled_tail(Instant::now(), true));
        }
    }

    #[test]
    fn settled_tail_callback_keeps_ownership_when_delivery_fails_mid_event() {
        let (mut tap, receiver, cursor) = tap_cursor_fixture(true);
        tap.shared
            .queue(Delivery::Subscribe(
                Arc::new(|_| {}),
                LockKeys::default(),
                false,
            ))
            .unwrap();
        let shared = tap.shared.clone();
        let delivery = std::thread::spawn(move || deliver(shared, receiver));
        let request = Arc::new(drag_request(&tap));
        let (reply, answer) = mpsc::channel();
        tap.begin_with(CaptureId(1), 1, request, reply, |tap, request| {
            tap.activate_begin(1, request, drag_prepared(), Some(|| {}))
        });
        answer
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let (entered, resume) = cursor.pause_next_warp();
        let shared = tap.shared.clone();
        let failure = std::thread::spawn(move || {
            entered.recv_timeout(Duration::from_secs(2)).unwrap();
            shared.fail();
            resume.send(()).unwrap();
        });
        let event = pointer(CGEventType::LeftMouseDragged, 0, false);
        CGEvent::set_location(Some(&event), CGPoint::new(70.0, 40.0));
        // SAFETY: the production callback borrows an unposted test event and fake tap state;
        // every cursor operation is injected, so there is no real tap or native mutation.
        let result = unsafe {
            tap_callback(
                ptr::null_mut(),
                CGEventType::LeftMouseDragged,
                (&*event as *const CGEvent).cast_mut(),
                (&mut tap as *mut TapState).cast(),
            )
        };
        failure.join().unwrap();
        let _ = tap.shared.output.send(Delivery::Stop);
        delivery.join().unwrap();
        assert!(tap.shared.dead.load(Ordering::Acquire));
        assert!(result.is_null());
    }

    #[test]
    fn settled_tail_survives_a_panic_while_handling_it() {
        let (mut tap, _receiver, cursor) = tap_cursor_fixture(true);
        tap.activate_begin(1, &drag_request(&tap), drag_prepared(), Some(|| {}))
            .unwrap();
        // The fake warp panics when its pause receiver is gone: a panic inside event() while it
        // handles one of the settled tail's drags.
        let (paused, _resume) = cursor.pause_next_warp();
        drop(paused);
        let event = pointer(CGEventType::LeftMouseDragged, 0, false);
        CGEvent::set_location(Some(&event), CGPoint::new(70.0, 40.0));
        // SAFETY: the production callback borrows an unposted test event and fake tap state;
        // every cursor operation is injected, so there is no real tap or native mutation.
        let result = unsafe {
            tap_callback(
                ptr::null_mut(),
                CGEventType::LeftMouseDragged,
                (&*event as *const CGEvent).cast_mut(),
                (&mut tap as *mut TapState).cast(),
            )
        };
        assert!(tap.shared.dead.load(Ordering::Acquire));
        assert!(result.is_null(), "a panic passed a settled drag to the OS");
        assert_eq!(tap.suppressed_buttons[0], 1);
    }

    #[test]
    fn dead_settled_tail_is_kept_while_the_button_is_held() {
        let (mut tap, _, _) = tap_cursor_fixture(true);
        tap.activate_begin(1, &drag_request(&tap), drag_prepared(), Some(|| {}))
            .unwrap();
        tap.shared.fail();
        let now = Instant::now();
        // Still held (HID down): no bound, however long the hold.
        assert!(tap.pending_settled_tail(now, false));
        assert!(tap.pending_settled_tail(now + Duration::from_secs(60), false));
        // Released in HID without the callback: one second, then the tap may go.
        let released = now + Duration::from_secs(61);
        assert!(tap.pending_settled_tail(released, true));
        assert!(!tap.pending_settled_tail(released + Duration::from_millis(1000), false));
        assert_eq!(tap.suppressed_buttons[0], 0);
    }

    #[test]
    fn hid_released_before_up_callback_does_not_retire_settled_suppression() {
        let (mut tap, _, _) = tap_cursor_fixture(true);
        tap.activate_begin(1, &drag_request(&tap), drag_prepared(), Some(|| {}))
            .unwrap();
        tap.shared.finish(EndReason::Requested, None).unwrap();
        let mut queries = Vec::new();
        tap.reconcile_buttons(Instant::now(), |source, number| {
            queries.push((source, number));
            false
        });
        assert_eq!(queries, [(CGEventSourceStateID::HIDSystemState, 0)]);
        assert!(tap.settled_primary);
        assert_eq!(tap.suppressed_buttons[0], 1);
        assert!(!tap.event(
            CGEventType::LeftMouseUp,
            &pointer(CGEventType::LeftMouseUp, 0, false)
        ));
        assert_eq!(tap.suppressed_buttons[0], 0);
        tap.activate_begin(2, &drag_request(&tap), drag_prepared(), Some(|| {}))
            .unwrap();
        tap.shared.finish(EndReason::Requested, None).unwrap();
        let now = Instant::now();
        tap.reconcile_buttons(now, |_, _| false);
        tap.reconcile_buttons(now + Duration::from_millis(1001), |_, _| false);
        assert_eq!(
            tap.suppressed_buttons[0], 0,
            "a missed callback is bounded to one second"
        );
    }

    #[test]
    fn drag_post_up_abort_preserves_tail_and_never_posts_another_up() {
        let (mut tap, receiver, _) = tap_cursor_fixture(true);
        let shared = tap.shared.clone();
        let request = Arc::new(drag_request(&tap));
        let posted = std::cell::Cell::new(0);
        let start = tap
            .activate_begin(
                1,
                &request,
                drag_prepared(),
                Some(|| {
                    posted.set(posted.get() + 1);
                    shared.finish(EndReason::Aborted, None).unwrap();
                }),
            )
            .unwrap();
        assert!(!tap.event(
            CGEventType::LeftMouseDragged,
            &pointer(CGEventType::LeftMouseDragged, 0, false)
        ));
        assert!(!tap.event(
            CGEventType::LeftMouseUp,
            &pointer(CGEventType::LeftMouseUp, 0, false)
        ));
        assert_eq!(tap.suppressed_buttons[0], 0);
        let (reply, answer) = mpsc::channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let out = events.clone();
        shared
            .queue(Delivery::Subscribe(
                Arc::new(move |event| out.lock().unwrap().push(event)),
                LockKeys::default(),
                false,
            ))
            .unwrap();
        shared
            .queue(Delivery::Activate {
                token: 1,
                id: CaptureId(1),
                start,
                request,
                reply,
            })
            .unwrap();
        shared.queue(Delivery::Stop).unwrap();
        deliver(shared, receiver);
        assert!(answer.recv().unwrap().is_err());
        assert_eq!(posted.get(), 1);
    }

    #[test]
    fn tap_detector_wiring_uses_fake_window_facts_and_releases_on_physical_up() {
        let (mut tap, receiver, _) = tap_cursor_fixture(true);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        tap.window_at = Box::new(move |point| {
            seen.fetch_add(1, Ordering::Relaxed);
            Ok(Some(drag::WindowFact {
                window: crosspane_types::id::WindowId(42),
                pid: 98126,
                scale: 1.0,
                frame: crosspane_types::geom::RectLogical::new(
                    crosspane_types::geom::PointLogical::new(point.x - 80.0, point.y - 12.0),
                    crosspane_types::geom::SizeLogical::new(400.0, 300.0),
                ),
            }))
        });
        for x in [40.0, 45.0, 99.0] {
            let event = pointer(CGEventType::LeftMouseDragged, 0, false);
            CGEvent::set_location(Some(&event), CGPoint::new(x, 50.0));
            assert!(tap.event(CGEventType::LeftMouseDragged, &event));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        let events: Vec<_> = receiver.try_iter().collect();
        assert_eq!(events.iter().filter(|event| matches!(event,
            Delivery::Event(0, CaptureEvent::DragAtEdge { window: crosspane_types::id::WindowId(42), grab, .. }) if *grab == PointDevice::new(80.0, 12.0)
        )).count(), 1);
        assert!(
            events.iter().all(|event| !matches!(
                event,
                Delivery::Event(0, CaptureEvent::EdgePressed { .. })
            ))
        );
        assert!(tap.event(
            CGEventType::LeftMouseUp,
            &pointer(CGEventType::LeftMouseUp, 0, false)
        ));
        assert!(receiver.try_iter().any(|event| matches!(
            event,
            Delivery::Event(
                0,
                CaptureEvent::EdgeReleased {
                    portal: PortalId(1),
                    ..
                }
            )
        )));
    }

    #[test]
    fn marked_settlement_up_passes_without_consuming_physical_tail() {
        let (mut tap, _, _) = tap_cursor_fixture(true);
        tap.activate_begin(1, &drag_request(&tap), drag_prepared(), Some(|| {}))
            .unwrap();
        assert!(tap.event(
            CGEventType::LeftMouseUp,
            &pointer(CGEventType::LeftMouseUp, 0, true)
        ));
        assert!(tap.settled_primary);
        assert_eq!(tap.suppressed_buttons[0], 1);
    }

    fn tap_cursor_fixture(pin_enabled: bool) -> (TapState, Receiver<Delivery>, Arc<FakeCursor>) {
        let (shared, receiver, cursor) = shared_cursor_fixture(pin_enabled);
        let display = Display {
            id: DisplayId(1),
            bounds: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(100.0, 100.0)),
            scale: 1.0,
        };
        let portals = Arc::new(vec![Portal {
            display,
            portal: CapturePortal {
                id: PortalId(1),
                display: display.id,
                edge: Edge::Right,
                from: 0.0,
                to: 100.0,
            },
        }]);
        *shared.portals.lock().unwrap() = portals.clone();
        let (_commands, commands) = mpsc::channel();
        (
            TapState {
                shared,
                commands,
                tap: None,
                portal_config: portals.clone(),
                portals,
                pressed: HashSet::new(),
                subscribed: true,
                monitor: false,
                last_activity: None,
                last_secure_poll: Instant::now(),
                last_geometry_poll: Instant::now(),
                blinded: false,
                lock_keys: LockKeys::default(),
                display: None,
                pin: None,
                local_keys: [false; 128],
                suppressed_keys: [0; 128],
                suppressed_buttons: [0; 256],
                held_buttons: HeldButtons::default(),
                drag: drag::Move::default(),
                window_at: Box::new(|_| Ok(None)),
                tiling_busy: Arc::new(AtomicBool::new(false)),
                settled_primary: false,
                settled_capture: 0,
                settled_tail_until: None,
            },
            receiver,
            cursor,
        )
    }

    fn activate_pin(tap: &mut TapState, scale: f64) -> CGPoint {
        let display = Display {
            scale,
            ..tap.portals[0].display
        };
        tap.display = Some(display);
        tap.shared
            .active
            .store(1 << 2 | EFFECTIVE, Ordering::Release);
        tap.shared.capturing.store(true, Ordering::Release);
        tap.start_pin(
            1,
            tap.shared.epoch.load(Ordering::Acquire),
            display,
            CGPoint::new(100.0, 50.0),
        )
        .unwrap();
        display.pin_point(CGPoint::new(100.0, 50.0)).unwrap()
    }

    fn motion_events(receiver: &Receiver<Delivery>) -> Vec<(u64, f64, f64, MotionKind)> {
        receiver
            .try_iter()
            .filter_map(|delivery| match delivery {
                Delivery::Event(token, CaptureEvent::Motion { dx, dy, kind, .. }) => {
                    Some((token, dx, dy, kind))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn captured_motion_pins_once_and_preserves_scaled_deltas() {
        let (mut tap, receiver, cursor) = tap_cursor_fixture(true);
        let point = activate_pin(&mut tap, 2.0);
        let event = pointer(CGEventType::MouseMoved, 0, false);
        CGEvent::set_double_value_field(Some(&event), CGEventField::MouseEventDeltaY, -3.0);
        for (location, warp_count) in [
            (CGPoint::new(75.0, 60.0), 1),
            (point, 1),
            (CGPoint::new(30.0, 80.0), 2),
        ] {
            CGEvent::set_location(Some(&event), location);
            assert!(!tap.event(CGEventType::MouseMoved, &event));
            assert_eq!(cursor.warps().len(), warp_count);
        }
        assert_eq!(cursor.warps(), [point, point]);
        assert_eq!(
            motion_events(&receiver),
            vec![
                (
                    1,
                    10.0,
                    -6.0,
                    MotionKind::Accelerated {
                        display: DisplayId(1)
                    }
                );
                3
            ]
        );
        assert_eq!(tap.shared.pin_metrics.motions.load(Ordering::Acquire), 3);
        assert_eq!(tap.shared.pin_metrics.warps.load(Ordering::Acquire), 2);
    }

    #[test]
    fn subscribed_motion_and_edge_hits_do_not_warp() {
        let (mut tap, receiver, cursor) = tap_cursor_fixture(true);
        let event = pointer(CGEventType::MouseMoved, 0, false);
        CGEvent::set_location(Some(&event), CGPoint::new(50.0, 50.0));
        assert!(tap.event(CGEventType::MouseMoved, &event));
        CGEvent::set_location(Some(&event), CGPoint::new(100.0, 50.0));
        assert!(tap.event(CGEventType::MouseMoved, &event));
        assert_eq!(edge_events(&receiver), [EdgeSeen::Pressed(1)]);
        assert!(cursor.warps().is_empty());
    }

    #[test]
    fn injected_motion_during_capture_does_not_warp() {
        let (mut tap, receiver, cursor) = tap_cursor_fixture(true);
        activate_pin(&mut tap, 2.0);
        for kind in [CGEventType::MouseMoved, CGEventType::LeftMouseDragged] {
            assert!(tap.event(kind, &pointer(kind, 0, true)));
        }
        assert!(cursor.warps().is_empty());
        assert!(motion_events(&receiver).is_empty());
        assert_eq!(tap.shared.pin_metrics.motions.load(Ordering::Acquire), 0);
    }

    #[test]
    fn ended_closed_gate_recovering_and_stale_pins_do_not_warp() {
        for state in ["ended", "gate", "recovering", "epoch", "token"] {
            let (mut tap, _receiver, cursor) = tap_cursor_fixture(true);
            activate_pin(&mut tap, 2.0);
            match state {
                "ended" => tap.shared.finish(EndReason::Requested, None).unwrap(),
                "gate" => tap.shared.gate.set_engine_permits(false),
                "recovering" => tap
                    .shared
                    .active
                    .store(1 << 2 | RECOVERING, Ordering::Release),
                "epoch" => {
                    tap.shared.epoch.fetch_add(1, Ordering::AcqRel);
                }
                "token" => tap
                    .shared
                    .active
                    .store(2 << 2 | EFFECTIVE, Ordering::Release),
                _ => unreachable!(),
            }
            tap.event(
                CGEventType::MouseMoved,
                &pointer(CGEventType::MouseMoved, 0, false),
            );
            assert!(cursor.warps().is_empty(), "state: {state}");
            // Also exercise the final authorization check without the event's capture snapshot.
            tap.pin_motion(1, CGPoint::new(80.0, 50.0));
            assert!(cursor.warps().is_empty(), "state: {state}");
            assert_eq!(tap.shared.pin_token.load(Ordering::Acquire), 0);
        }
    }

    #[test]
    fn captured_drags_pin_and_preserve_button_suppression() {
        for (down, dragged, up, button) in [
            (
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseDragged,
                CGEventType::LeftMouseUp,
                0,
            ),
            (
                CGEventType::RightMouseDown,
                CGEventType::RightMouseDragged,
                CGEventType::RightMouseUp,
                1,
            ),
            (
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseDragged,
                CGEventType::OtherMouseUp,
                3,
            ),
        ] {
            let (mut tap, receiver, cursor) = tap_cursor_fixture(true);
            let point = activate_pin(&mut tap, 2.0);
            assert!(!tap.event(down, &pointer(down, button, false)));
            assert_eq!(tap.suppressed_buttons[button as usize], 1);
            assert!(!tap.event(dragged, &pointer(dragged, button, false)));
            assert!(!tap.event(up, &pointer(up, button, false)));
            assert_eq!(cursor.warps(), [point]);
            assert_eq!(
                motion_events(&receiver),
                [(
                    1,
                    10.0,
                    0.0,
                    MotionKind::Accelerated {
                        display: DisplayId(1)
                    }
                )]
            );
        }
    }

    #[test]
    fn release_clears_pin_before_cursor_operations_and_late_motion() {
        let (mut tap, receiver, cursor) = tap_cursor_fixture(true);
        activate_pin(&mut tap, 2.0);
        // Fake the foreground operations as well: no Quartz cursor API is used by this fixture.
        (tap.shared.cursor.associate)(false).unwrap();
        tap.shared.detached.store(true, Ordering::Release);
        (tap.shared.cursor.hide)().unwrap();
        tap.shared.hidden.store(true, Ordering::Release);
        cursor.calls.lock().unwrap().clear();
        tap.shared
            .finish(
                EndReason::Requested,
                Some((DisplayId(1), PointDevice::new(20.0, 40.0))),
            )
            .unwrap();
        let expected = vec![
            (CursorCall::Associate(true), 0),
            (CursorCall::Warp(CGPoint::new(10.0, 20.0)), 0),
            (CursorCall::Show, 0),
        ];
        assert_eq!(*cursor.calls.lock().unwrap(), expected);
        tap.event(
            CGEventType::MouseMoved,
            &pointer(CGEventType::MouseMoved, 0, false),
        );
        tap.pin_motion(1, CGPoint::new(80.0, 50.0));
        assert_eq!(*cursor.calls.lock().unwrap(), expected);
        assert!(tap.pin.is_none());
        assert!(motion_events(&receiver).is_empty());
    }

    struct PausedPin {
        shared: Arc<Shared>,
        cursor: Arc<FakeCursor>,
        receiver: Receiver<Delivery>,
        resume: Sender<()>,
        tap: std::thread::JoinHandle<()>,
    }

    fn paused_pin() -> PausedPin {
        let (ready, fixture) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            // Build tap-only CF objects on this thread; no event is posted and every cursor
            // operation is fake, including the foreground restore path.
            let (mut tap, receiver, cursor) = tap_cursor_fixture(true);
            activate_pin(&mut tap, 2.0);
            (tap.shared.cursor.associate)(false).unwrap();
            tap.shared.detached.store(true, Ordering::Release);
            (tap.shared.cursor.hide)().unwrap();
            tap.shared.hidden.store(true, Ordering::Release);
            cursor.calls.lock().unwrap().clear();
            *cursor.position.lock().unwrap() = Some(CGPoint::new(100.0, 50.0));
            let (paused, resume) = cursor.pause_next_warp();
            ready
                .send((tap.shared.clone(), cursor, receiver, paused, resume))
                .unwrap();
            assert!(!tap.event(
                CGEventType::MouseMoved,
                &pointer(CGEventType::MouseMoved, 0, false)
            ));
            // An event queued after recovery must not submit another pin.
            tap.event(
                CGEventType::MouseMoved,
                &pointer(CGEventType::MouseMoved, 0, false),
            );
        });
        let (shared, cursor, receiver, paused, resume) =
            fixture.recv_timeout(Duration::from_secs(1)).unwrap();
        paused.recv_timeout(Duration::from_secs(1)).unwrap();
        PausedPin {
            shared,
            cursor,
            receiver,
            resume,
            tap: thread,
        }
    }

    fn assert_ended_once(receiver: Receiver<Delivery>) {
        // Feed the actual race's queued messages through the production delivery state machine.
        // A watchdog may repeat an internal End message; only one public Ended may be sent.
        let (shared, deliveries) = shared_fixture();
        shared.active.store(1 << 2 | PENDING, Ordering::Release);
        shared.capturing.store(true, Ordering::Release);
        let (events, seen) = mpsc::channel();
        shared
            .queue(Delivery::Subscribe(
                Arc::new(move |event| {
                    events.send(event).unwrap();
                }),
                LockKeys::default(),
                false,
            ))
            .unwrap();
        let (reply, _answer) = mpsc::channel();
        shared
            .queue(Delivery::Activate {
                token: 1,
                id: CaptureId(1),
                start: CaptureStart {
                    held_keys: Vec::new(),
                    lock_keys: LockKeys::default(),
                },
                request: Arc::new(Request {
                    deadline: Instant::now() + Duration::from_secs(1),
                    cancelled: AtomicBool::new(false),
                    epoch: 0,
                }),
                reply,
            })
            .unwrap();
        for message in receiver.try_iter() {
            shared.queue(message).unwrap();
        }
        shared.queue(Delivery::Stop).unwrap();
        deliver(shared, deliveries);
        assert_eq!(
            seen.try_iter()
                .filter(|event| matches!(
                    event,
                    CaptureEvent::Ended {
                        id: CaptureId(1),
                        ..
                    }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn in_flight_pin_drains_before_deferred_release_and_a_new_token() {
        let PausedPin {
            shared,
            cursor,
            receiver,
            resume,
            tap,
        } = paused_pin();
        let finishing = shared.clone();
        let (done, finished) = mpsc::channel();
        let end = std::thread::spawn(move || {
            let started = Instant::now();
            let result = finishing.finish(
                EndReason::Requested,
                Some((DisplayId(1), PointDevice::new(20.0, 40.0))),
            );
            done.send((result, started.elapsed())).unwrap();
        });
        let (result, elapsed) = finished.recv_timeout(CALL_BUDGET * 4).unwrap();
        assert!(matches!(result, Err(PlatformError::Timeout)));
        assert!(elapsed < CALL_BUDGET);
        eprintln!("deferred end total elapsed: {elapsed:?}");
        assert_eq!(shared.pin_token.load(Ordering::Acquire), 0);
        assert!(!shared.detached.load(Ordering::Acquire));
        assert!(!shared.hidden.load(Ordering::Acquire));
        assert_eq!(shared.active.load(Ordering::Acquire), 1 << 2 | RECOVERING);
        assert!(
            shared
                .active
                .compare_exchange(0, 2 << 2 | PENDING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        );
        // The old pin call is inside the fake but has not landed. No release was submitted yet.
        assert_eq!(cursor.warps(), [CGPoint::new(99.5, 50.0)]);
        assert_eq!(
            *cursor.position.lock().unwrap(),
            Some(CGPoint::new(100.0, 50.0))
        );
        resume.send(()).unwrap();
        tap.join().unwrap();
        end.join().unwrap();
        assert_eq!(
            cursor.warps(),
            [CGPoint::new(99.5, 50.0), CGPoint::new(10.0, 20.0)]
        );
        assert_eq!(
            *cursor.position.lock().unwrap(),
            Some(CGPoint::new(10.0, 20.0))
        );
        assert_eq!(shared.active.load(Ordering::Acquire), 0);
        assert_eq!(shared.pin_inflight.load(Ordering::Acquire), 0);
        assert_eq!(shared.pin_finish.load(Ordering::Acquire), 0);
        assert!(
            shared
                .active
                .compare_exchange(0, 2 << 2 | PENDING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        );
        let calls = cursor.calls.lock().unwrap();
        assert_eq!(
            calls.last(),
            Some(&(CursorCall::Warp(CGPoint::new(10.0, 20.0)), 0))
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(call, _)| matches!(call, CursorCall::Show))
                .count(),
            1
        );
        assert_ended_once(receiver);
    }

    #[test]
    fn in_flight_pin_abort_and_closed_gate_recover_without_waiting() {
        for closed in [false, true] {
            let PausedPin {
                shared,
                cursor,
                receiver,
                resume,
                tap,
            } = paused_pin();
            if closed {
                shared.gate.set_engine_permits(false);
            }
            let recovering = shared.clone();
            let (done, finished) = mpsc::channel();
            let abort = std::thread::spawn(move || {
                let started = Instant::now();
                if closed {
                    recovering.finish(EndReason::Lost, None).unwrap();
                } else {
                    Abort(recovering).abort();
                }
                done.send(started.elapsed()).unwrap();
            });
            let elapsed = finished.recv_timeout(CALL_BUDGET * 4).unwrap();
            assert!(elapsed < CALL_BUDGET);
            assert_eq!(shared.pin_token.load(Ordering::Acquire), 0);
            assert_eq!(shared.active.load(Ordering::Acquire), 1 << 2 | RECOVERING);
            assert!(!shared.detached.load(Ordering::Acquire));
            assert!(!shared.hidden.load(Ordering::Acquire));
            // This was submitted while authorized, before abort/gate closure. No new warp
            // (including a corrective warp into a closed gate) is submitted during recovery.
            assert_eq!(cursor.warps(), [CGPoint::new(99.5, 50.0)]);
            resume.send(()).unwrap();
            tap.join().unwrap();
            abort.join().unwrap();
            assert_eq!(cursor.warps(), [CGPoint::new(99.5, 50.0)]);
            assert_eq!(
                *cursor.position.lock().unwrap(),
                Some(CGPoint::new(99.5, 50.0))
            );
            assert_eq!(shared.active.load(Ordering::Acquire), 0);
            assert_eq!(shared.pin_inflight.load(Ordering::Acquire), 0);
            assert_ended_once(receiver);
        }
    }

    #[test]
    fn watchdog_and_closed_gate_cannot_overtake_a_pending_release() {
        for closed in [false, true] {
            let PausedPin {
                shared,
                cursor,
                receiver,
                resume,
                tap,
            } = paused_pin();
            assert!(matches!(
                shared.finish(
                    EndReason::Requested,
                    Some((DisplayId(1), PointDevice::new(20.0, 40.0))),
                ),
                Err(PlatformError::Timeout)
            ));
            if closed {
                shared.gate.set_engine_permits(false);
            }
            let started = Instant::now();
            shared.finish(EndReason::Aborted, None).unwrap();
            assert!(started.elapsed() < CALL_BUDGET);
            assert_eq!(shared.active.load(Ordering::Acquire), 1 << 2 | RECOVERING);
            assert_eq!(cursor.warps(), [CGPoint::new(99.5, 50.0)]);
            resume.send(()).unwrap();
            tap.join().unwrap();
            let expected = if closed {
                vec![CGPoint::new(99.5, 50.0)]
            } else {
                vec![CGPoint::new(99.5, 50.0), CGPoint::new(10.0, 20.0)]
            };
            assert_eq!(cursor.warps(), expected);
            assert_eq!(*cursor.position.lock().unwrap(), expected.last().copied());
            assert_eq!(shared.active.load(Ordering::Acquire), 0);
            assert_ended_once(receiver);
        }
    }

    #[test]
    fn pin_drain_before_finish_publication_releases_once_and_emits_ended_once() {
        let PausedPin {
            shared,
            cursor,
            receiver,
            resume,
            tap,
        } = paused_pin();
        let (showing, show_resume) = cursor.pause_next_show();
        let finishing = shared.clone();
        let end = std::thread::spawn(move || {
            finishing.finish(
                EndReason::Requested,
                Some((DisplayId(1), PointDevice::new(20.0, 40.0))),
            )
        });
        // Show is after the in-flight branch is selected, before the finish is published.
        showing.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(shared.pin_finish.load(Ordering::Acquire), 0);
        resume.send(()).unwrap();
        tap.join().unwrap();
        assert_eq!(shared.pin_inflight.load(Ordering::Acquire), 0);
        assert_eq!(shared.pin_finish.load(Ordering::Acquire), 0);
        assert_eq!(shared.active.load(Ordering::Acquire), 1 << 2 | RECOVERING);
        assert_eq!(cursor.warps(), [CGPoint::new(99.5, 50.0)]);
        // Publication now observes the drained pin; the caller claims the finish exactly once.
        show_resume.send(()).unwrap();
        assert!(matches!(end.join().unwrap(), Err(PlatformError::Timeout)));
        shared.complete_pin_finish(1).unwrap();
        assert_eq!(
            cursor.warps(),
            [CGPoint::new(99.5, 50.0), CGPoint::new(10.0, 20.0)]
        );
        assert_eq!(
            *cursor.position.lock().unwrap(),
            Some(CGPoint::new(10.0, 20.0))
        );
        assert!(!shared.hidden.load(Ordering::Acquire));
        assert!(!shared.detached.load(Ordering::Acquire));
        assert_eq!(shared.active.load(Ordering::Acquire), 0);
        assert_eq!(shared.pin_finish.load(Ordering::Acquire), 0);
        assert_ended_once(receiver);
    }

    #[test]
    fn pin_warp_failure_disables_only_that_token_and_preserves_capture() {
        let (mut tap, receiver, cursor) = tap_cursor_fixture(true);
        activate_pin(&mut tap, 2.0);
        cursor.fail_warps.store(1, Ordering::Release);
        for _ in 0..3 {
            assert!(!tap.event(
                CGEventType::MouseMoved,
                &pointer(CGEventType::MouseMoved, 0, false)
            ));
        }
        assert_eq!(cursor.warps(), [CGPoint::new(99.5, 50.0)]);
        assert_eq!(cursor.fail_warps.load(Ordering::Acquire), 0);
        assert_eq!(
            tap.shared.active.load(Ordering::Acquire),
            1 << 2 | EFFECTIVE
        );
        assert!(tap.shared.capturing.load(Ordering::Acquire));
        assert_eq!(tap.shared.pin_token.load(Ordering::Acquire), 0);
        assert_eq!(
            motion_events(&receiver),
            vec![
                (
                    1,
                    10.0,
                    0.0,
                    MotionKind::Accelerated {
                        display: DisplayId(1)
                    }
                );
                3
            ]
        );
        tap.shared.finish(EndReason::Requested, None).unwrap();
        tap.shared
            .active
            .store(2 << 2 | EFFECTIVE, Ordering::Release);
        tap.shared.capturing.store(true, Ordering::Release);
        tap.start_pin(
            2,
            tap.shared.epoch.load(Ordering::Acquire),
            tap.display.unwrap(),
            CGPoint::new(100.0, 50.0),
        )
        .unwrap();
        assert!(!tap.event(
            CGEventType::MouseMoved,
            &pointer(CGEventType::MouseMoved, 0, false)
        ));
        assert_eq!(cursor.warps(), [CGPoint::new(99.5, 50.0); 2]);
        assert_eq!(
            motion_events(&receiver),
            [(
                2,
                10.0,
                0.0,
                MotionKind::Accelerated {
                    display: DisplayId(1)
                }
            )]
        );
        assert_eq!(tap.shared.pin_metrics.motions.load(Ordering::Acquire), 1);
        assert_eq!(tap.shared.pin_metrics.warps.load(Ordering::Acquire), 1);
    }

    #[test]
    fn cursor_pin_default_on_and_zero_kill_switch_restores_motion() {
        assert!(cursor_pin_enabled(None));
        assert!(cursor_pin_enabled(Some("1")));
        assert!(cursor_pin_enabled(Some("other")));
        assert!(!cursor_pin_enabled(Some("0")));
        for value in [None, Some("0")] {
            let (mut tap, receiver, cursor) = tap_cursor_fixture(cursor_pin_enabled(value));
            let point = activate_pin(&mut tap, 2.0);
            assert!(!tap.event(
                CGEventType::MouseMoved,
                &pointer(CGEventType::MouseMoved, 0, false)
            ));
            if value.is_none() {
                assert_eq!(cursor.warps(), [point]);
            } else {
                assert!(cursor.warps().is_empty());
                assert!(tap.pin.is_none());
                assert_eq!(tap.shared.pin_token.load(Ordering::Acquire), 0);
            }
            assert_eq!(
                motion_events(&receiver),
                [(
                    1,
                    10.0,
                    0.0,
                    MotionKind::Accelerated {
                        display: DisplayId(1)
                    }
                )]
            );
        }
    }

    #[test]
    fn cursor_pin_clamps_crossing_point_inside_its_display() {
        let display = Display {
            id: DisplayId(9),
            bounds: CGRect::new(CGPoint::new(-100.0, -50.0), CGSize::new(100.0, 100.0)),
            scale: 2.0,
        };
        for (location, expected) in [
            (CGPoint::new(0.0, 50.0), CGPoint::new(-0.5, 49.5)),
            (CGPoint::new(-110.0, -60.0), CGPoint::new(-100.0, -50.0)),
            (CGPoint::new(-20.0, 10.0), CGPoint::new(-20.0, 10.0)),
        ] {
            assert_eq!(display.pin_point(location).unwrap(), expected);
        }
        assert!(display.pin_point(CGPoint::new(f64::NAN, 0.0)).is_err());
    }

    #[test]
    fn cursor_pin_measurements_reset_for_each_token_and_track_longest_gap() {
        let (mut tap, _receiver, _cursor) = tap_cursor_fixture(true);
        activate_pin(&mut tap, 2.0);
        let metrics = &tap.shared.pin_metrics;
        for at in [1, 10_000_001, 260_000_001, 265_000_001] {
            metrics.motion(MonoTime::from_nanos(at));
        }
        assert_eq!(metrics.motions.load(Ordering::Acquire), 4);
        assert_eq!(metrics.longest_gap.load(Ordering::Acquire), 250_000_000);
        tap.shared.finish(EndReason::Requested, None).unwrap();
        tap.shared.active.store(2 << 2 | PENDING, Ordering::Release);
        tap.shared.capturing.store(true, Ordering::Release);
        tap.start_pin(
            2,
            tap.shared.epoch.load(Ordering::Acquire),
            tap.display.unwrap(),
            CGPoint::new(100.0, 50.0),
        )
        .unwrap();
        assert_eq!(tap.shared.pin_token.load(Ordering::Acquire), 2);
        assert_eq!(tap.shared.pin_metrics.motions.load(Ordering::Acquire), 0);
        assert_eq!(tap.shared.pin_metrics.warps.load(Ordering::Acquire), 0);
        assert_eq!(
            tap.shared.pin_metrics.last_motion.load(Ordering::Acquire),
            0
        );
        assert_eq!(
            tap.shared.pin_metrics.longest_gap.load(Ordering::Acquire),
            0
        );
    }

    /// A synthetic pointer event at the right edge (x = 100) of the fixture display.
    fn pointer(kind: CGEventType, button: u32, injected: bool) -> CFRetained<CGEvent> {
        let event =
            CGEvent::new_mouse_event(None, kind, CGPoint::new(100.0, 50.0), CGMouseButton(button))
                .expect("synthetic mouse event");
        CGEvent::set_double_value_field(Some(&event), CGEventField::MouseEventDeltaX, 5.0);
        if injected {
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::EventSourceUserData,
                INJECTED,
            );
        }
        event
    }

    #[derive(Debug, PartialEq)]
    enum EdgeSeen {
        Pressed(u32),
        Released(u32),
    }

    fn edge_events(receiver: &Receiver<Delivery>) -> Vec<EdgeSeen> {
        receiver
            .try_iter()
            .filter_map(|delivery| match delivery {
                Delivery::Event(0, CaptureEvent::EdgePressed { portal, .. }) => {
                    Some(EdgeSeen::Pressed(portal.0))
                }
                Delivery::Event(0, CaptureEvent::EdgeReleased { portal, .. }) => {
                    Some(EdgeSeen::Released(portal.0))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tap_drag_at_the_edge_never_presses_and_delivery_is_native() {
        for (down, dragged, up, button) in [
            (
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseDragged,
                CGEventType::LeftMouseUp,
                0,
            ),
            (
                CGEventType::RightMouseDown,
                CGEventType::RightMouseDragged,
                CGEventType::RightMouseUp,
                1,
            ),
            (
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseDragged,
                CGEventType::OtherMouseUp,
                3,
            ),
        ] {
            let (mut tap, receiver) = tap_fixture();
            // Pointer reaches the edge: pressed.
            assert!(tap.event(
                CGEventType::MouseMoved,
                &pointer(CGEventType::MouseMoved, 0, false)
            ));
            assert_eq!(edge_events(&receiver), [EdgeSeen::Pressed(1)]);
            // Button goes down at the edge: released at once, event still delivered to the OS.
            assert!(tap.event(down, &pointer(down, button, false)));
            assert_eq!(edge_events(&receiver), [EdgeSeen::Released(1)]);
            assert!(tap.pressed.is_empty());
            // The drag runs along the edge: no press, every event delivered to the OS.
            for _ in 0..3 {
                assert!(tap.event(dragged, &pointer(dragged, button, false)));
            }
            assert!(edge_events(&receiver).is_empty());
            // The up at the edge presses nothing and is delivered to the OS.
            assert!(tap.event(up, &pointer(up, button, false)));
            assert!(edge_events(&receiver).is_empty());
            assert!(!tap.held_buttons.any());
            // Only later motion presses again.
            assert!(tap.event(
                CGEventType::MouseMoved,
                &pointer(CGEventType::MouseMoved, 0, false)
            ));
            assert_eq!(edge_events(&receiver), [EdgeSeen::Pressed(1)]);
        }
    }

    #[test]
    fn tap_ignores_injected_pointer_events() {
        let (mut tap, receiver) = tap_fixture();
        // Injected motion at the edge neither presses nor drags.
        for kind in [
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::OtherMouseDragged,
        ] {
            assert!(tap.event(kind, &pointer(kind, 0, true)));
        }
        assert!(edge_events(&receiver).is_empty());
        // A real press, then an injected down and up: no release, and no held button recorded.
        assert!(tap.event(
            CGEventType::MouseMoved,
            &pointer(CGEventType::MouseMoved, 0, false)
        ));
        assert_eq!(edge_events(&receiver), [EdgeSeen::Pressed(1)]);
        for kind in [CGEventType::LeftMouseDown, CGEventType::RightMouseDown] {
            assert!(tap.event(kind, &pointer(kind, 0, true)));
        }
        assert!(edge_events(&receiver).is_empty());
        assert!(!tap.held_buttons.any());
        assert_eq!(tap.pressed, ids(&[1]));
        // And an injected up cannot clear a button the user really holds.
        assert!(tap.event(
            CGEventType::LeftMouseDown,
            &pointer(CGEventType::LeftMouseDown, 0, false)
        ));
        assert_eq!(edge_events(&receiver), [EdgeSeen::Released(1)]);
        assert!(tap.event(
            CGEventType::LeftMouseUp,
            &pointer(CGEventType::LeftMouseUp, 0, true)
        ));
        assert!(tap.held_buttons.get(0));
    }

    #[test]
    fn tap_tracks_a_button_pressed_during_capture() {
        let (mut tap, receiver) = tap_fixture();
        let display = tap.portals[0].display;
        tap.display = Some(display);
        tap.shared
            .active
            .store(1 << 2 | EFFECTIVE, Ordering::Release);
        tap.shared.capturing.store(true, Ordering::Release);
        // Captured: the down is swallowed and remembered.
        assert!(!tap.event(
            CGEventType::LeftMouseDown,
            &pointer(CGEventType::LeftMouseDown, 0, false)
        ));
        assert!(tap.held_buttons.get(0));
        assert_eq!(tap.suppressed_buttons[0], 1);
        // Capture ends with the button still down.
        tap.shared.finish(EndReason::Requested, None).unwrap();
        let _ = edge_events(&receiver);
        assert!(tap.event(
            CGEventType::LeftMouseDragged,
            &pointer(CGEventType::LeftMouseDragged, 0, false)
        ));
        assert!(edge_events(&receiver).is_empty());
        // The suppressed up is still swallowed, then the button no longer blocks a press.
        assert!(!tap.event(
            CGEventType::LeftMouseUp,
            &pointer(CGEventType::LeftMouseUp, 0, false)
        ));
        assert!(!tap.held_buttons.any());
        assert!(tap.event(
            CGEventType::MouseMoved,
            &pointer(CGEventType::MouseMoved, 0, false)
        ));
        assert_eq!(edge_events(&receiver), [EdgeSeen::Pressed(1)]);
    }

    #[test]
    fn flags_changed_modifier_down_up() {
        for (left, right, left_bit, right_bit, aggregate) in [
            (0x38, 0x3c, 0x2, 0x4, CGEventFlags::MaskShift),
            (0x3b, 0x3e, 0x1, 0x2000, CGEventFlags::MaskControl),
            (0x3a, 0x3d, 0x20, 0x40, CGEventFlags::MaskAlternate),
            (0x37, 0x36, 0x8, 0x10, CGEventFlags::MaskCommand),
        ] {
            let both = aggregate | CGEventFlags::from_bits_retain(left_bit | right_bit);
            for code in [left, right] {
                assert_eq!(modifier_down(code, both), Some(true));
                assert_eq!(modifier_down(code, CGEventFlags::empty()), Some(false));
            }
            let right_only = aggregate | CGEventFlags::from_bits_retain(right_bit);
            assert_eq!(modifier_down(left, right_only), Some(false));
            assert_eq!(modifier_down(right, right_only), Some(true));
            let left_only = aggregate | CGEventFlags::from_bits_retain(left_bit);
            assert_eq!(modifier_down(left, left_only), Some(true));
            assert_eq!(modifier_down(right, left_only), Some(false));
        }
        assert_eq!(
            modifier_down(0x3f, CGEventFlags::MaskSecondaryFn),
            Some(true)
        );
        assert_eq!(modifier_down(0x3f, CGEventFlags::empty()), Some(false));
        assert_eq!(modifier_down(0x39, CGEventFlags::MaskAlphaShift), None);
        assert_eq!(modifier_down(0, CGEventFlags::MaskShift), None);
    }

    #[test]
    fn button_mapping() {
        for (number, mapped) in [
            MouseButton::PRIMARY,
            MouseButton::SECONDARY,
            MouseButton::TERTIARY,
            MouseButton::BACK,
            MouseButton::FORWARD,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(button(number as i64), Some(mapped));
        }
        assert_eq!(button(7), Some(MouseButton(8)));
        assert_eq!(button(-1), None);
        assert_eq!(button(255), None);
    }

    #[test]
    fn scroll_conversion() {
        let discrete = scroll(false, 1.0, -2.0, 0, 0);
        assert_eq!((discrete.v120_x, discrete.v120_y), (-120, -240));
        assert_eq!(discrete.pixels, None);
        assert_eq!(discrete.phase, ScrollPhase::Discrete);
        assert_eq!(scroll(true, 0.0, 0.0, 0, 0).phase, ScrollPhase::Discrete);
        for (phase, momentum, expected) in [
            (128, 0, ScrollPhase::MayBegin),
            (1, 0, ScrollPhase::Began),
            (2, 0, ScrollPhase::Changed),
            (4, 0, ScrollPhase::Ended),
            (8, 0, ScrollPhase::Cancelled),
            (0, 1, ScrollPhase::MomentumBegan),
            (0, 2, ScrollPhase::MomentumChanged),
            (0, 3, ScrollPhase::MomentumEnded),
        ] {
            let converted = scroll(true, -3.0, 4.0, phase, momentum);
            assert_eq!(converted.pixels, Some(VectorLogical::new(3.0, 4.0)));
            assert_eq!((converted.v120_x, converted.v120_y), (0, 0));
            assert_eq!(converted.phase, expected);
            assert_eq!(scroll(true, 0.0, 0.0, phase, momentum).phase, expected);
        }
    }

    #[test]
    fn timestamp_detection() {
        let now = 62_058_106_614_000;
        let ticks = now * 3 / 125;
        assert_eq!(
            timestamp_interpretation(ticks, ticks * 125 / 3, now),
            MACH_TICKS
        );
        assert_eq!(
            timestamp_interpretation(now, now * 125 / 3, now),
            NANOSECONDS
        );
        assert_eq!(timestamp_interpretation(0, 0, now), UNKNOWN_TIME);
        assert_eq!(timestamp_interpretation(42, 1_750, now), UNKNOWN_TIME);
        assert_eq!(
            timestamp_interpretation(now + 1_000_000_000, 0, now),
            UNKNOWN_TIME
        );
    }
}
