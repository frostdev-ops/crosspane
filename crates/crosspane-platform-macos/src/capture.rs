//! Session event-tap capture. Mutable input state stays on the tap run loop; abort
//! only touches atomics and public cursor APIs. No AppKit or private APIs.

use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
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
    CGAssociateMouseAndMouseCursorPosition, CGDisplayBounds, CGDisplayHideCursor,
    CGDisplayPixelsWide, CGDisplayShowCursor, CGError, CGEvent, CGEventField, CGEventFlags,
    CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CGGetDisplaysWithPoint, CGMouseButton,
    CGWarpMouseCursorPosition, kCGNullDirectDisplay,
};

use crate::{clock, permissions};

const CALL_BUDGET: Duration = Duration::from_millis(50);
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

#[derive(Clone, Copy, Debug)]
struct Display {
    id: DisplayId,
    bounds: CGRect,
    scale: f64,
}

impl Display {
    fn read(id: DisplayId) -> Result<Self, PlatformError> {
        let bounds = CGDisplayBounds(id.0);
        let pixels = CGDisplayPixelsWide(id.0) as f64;
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

fn modifier_down(keycode: u16, flags: CGEventFlags) -> Option<bool> {
    let mask = match keycode {
        0x38 | 0x3c => CGEventFlags::MaskShift,
        0x3b | 0x3e => CGEventFlags::MaskControl,
        0x3a | 0x3d => CGEventFlags::MaskAlternate,
        0x37 | 0x36 => CGEventFlags::MaskCommand,
        0x3f => CGEventFlags::MaskSecondaryFn,
        _ => return None,
    };
    Some(flags.contains(mask))
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

fn scroll(continuous: bool, x: f64, y: f64, scale: f64, phase: i64, momentum: i64) -> ScrollDelta {
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
        // Keep Quartz's sign and natural-scrolling transformation: no second inversion.
        v120_x: if continuous { 0 } else { (x * 120.0) as i32 },
        v120_y: if continuous { 0 } else { (y * 120.0) as i32 },
        pixels: continuous.then(|| VectorLogical::new(x * scale, y * scale)),
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
    detached: AtomicBool,
    hidden: AtomicBool,
    stop: AtomicBool,
    output: Sender<Delivery>,
    time: AtomicU8,
    portals: Mutex<Arc<Vec<Portal>>>,
}

impl Shared {
    fn event(&self, token: u64, event: CaptureEvent) {
        if !self.stop.load(Ordering::Acquire) {
            let _ = self.output.send(Delivery::Event(token, event));
        }
    }

    /// No application locks or wait for the tap. Also cancels queued activation.
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
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
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
                if reason == EndReason::Aborted && active & 3 == RECOVERING {
                    self.capturing.store(false, Ordering::Release);
                    let result = cg_result(
                        CGAssociateMouseAndMouseCursorPosition(true),
                        "abort associate cursor",
                    );
                    let shown = if self.hidden.swap(false, Ordering::AcqRel) {
                        cg_result(
                            CGDisplayShowCursor(kCGNullDirectDisplay),
                            "abort show cursor",
                        )
                    } else {
                        Ok(())
                    };
                    let _ = self.output.send(Delivery::End(active >> 2, reason));
                    return result.and(shown);
                }
                return Ok(());
            }
        };
        if expected.is_some() {
            self.epoch.fetch_add(1, Ordering::AcqRel);
        }
        self.capturing.store(false, Ordering::Release);
        let mut result = Ok(());
        if self.detached.swap(false, Ordering::AcqRel) || active != 0 {
            result = cg_result(
                CGAssociateMouseAndMouseCursorPosition(true),
                "associate cursor",
            );
        }
        if let Some((display, point)) = warp {
            let warped = Display::read(display)
                .and_then(|display| display.global(point))
                .and_then(|point| {
                    if self.gate.is_open() {
                        cg_result(CGWarpMouseCursorPosition(point), "warp cursor")
                    } else {
                        Err(PlatformError::Locked)
                    }
                });
            if result.is_ok() {
                result = warped;
            }
        }
        if self.hidden.swap(false, Ordering::AcqRel) {
            let shown = cg_result(CGDisplayShowCursor(kCGNullDirectDisplay), "show cursor");
            if result.is_ok() {
                result = shown;
            }
        }
        if active & 3 == EFFECTIVE {
            let _ = self.output.send(Delivery::End(active >> 2, reason));
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

fn deliver(shared: Arc<Shared>, receiver: Receiver<Delivery>) {
    let mut sink: Option<Arc<dyn EventSink<CaptureEvent>>> = None;
    let mut active: Option<(u64, CaptureId)> = None;
    while let Ok(message) = receiver.recv() {
        match message {
            Delivery::Subscribe(new_sink, locks, blinded) => {
                new_sink.send(CaptureEvent::LockKeys(locks));
                new_sink.send(CaptureEvent::KeyboardBlinded(blinded));
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
                        sink.send(CaptureEvent::Started { id });
                        Ok(start)
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
                {
                    sink.send(event);
                }
            }
            Delivery::End(token, reason) => {
                if let Some((current, id)) = active
                    && current == token
                {
                    if let Some(sink) = &sink {
                        sink.send(CaptureEvent::Ended { id, reason });
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
    Monitor(bool, Arc<Request>, Sender<Result<(), PlatformError>>),
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
        let (output, events) = mpsc::channel();
        let shared = Arc::new(Shared {
            gate,
            output,
            active: AtomicU64::new(0),
            capturing: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            detached: AtomicBool::new(false),
            hidden: AtomicBool::new(false),
            stop: AtomicBool::new(false),
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
            let _ = shared.output.send(Delivery::Stop);
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
                let _ = shared.output.send(Delivery::Stop);
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
        self.commands
            .send(command)
            .map_err(|_| PlatformError::Backend("capture thread stopped".into()))
    }
}

impl InputCapture for MacCapture {
    fn set_portals(&mut self, portals: &[CapturePortal]) -> Result<(), PlatformError> {
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
        let _ = self.shared.output.send(Delivery::Stop);
        // No join: drop/watchdog must work even if the tap is stuck.
    }
}

struct TapState {
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    tap: Option<CFRetained<CFMachPort>>,
    portals: Arc<Vec<Portal>>,
    pressed: HashSet<PortalId>,
    subscribed: bool,
    monitor: bool,
    last_activity: Option<MonoTime>,
    blinded: bool,
    lock_keys: LockKeys,
    display: Option<Display>,
    local_keys: [bool; 128],
    suppressed_keys: [u64; 128],
    suppressed_buttons: [u64; 256],
}

impl TapState {
    fn sync_portals(&mut self) {
        // The callback never waits for a writer. Immutable snapshots keep hit testing fast.
        let portals = match self.shared.portals.try_lock() {
            Ok(current) => current.clone(),
            Err(_) => return,
        };
        if Arc::ptr_eq(&self.portals, &portals) {
            return;
        }
        let at = clock::now();
        self.pressed.retain(|id| {
            let old = self.portals.iter().find(|p| p.portal.id == *id);
            let new = portals.iter().find(|p| p.portal.id == *id);
            let keep = old
                .zip(new)
                .is_some_and(|(old, new)| old.portal == new.portal);
            if !keep {
                self.shared
                    .event(0, CaptureEvent::EdgeReleased { portal: *id, at });
            }
            keep
        });
        self.portals = portals;
    }

    fn release_edges(&mut self, at: MonoTime) {
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
        request: Arc<Request>,
        reply: Sender<Result<CaptureStart, PlatformError>>,
    ) {
        self.sync_portals();
        let result = self.prepare_begin(portal, token, &request);
        match result {
            Ok(start) => {
                let _ = self.shared.output.send(Delivery::Activate {
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
                    if self.shared.detached.swap(false, Ordering::AcqRel) {
                        let _ = CGAssociateMouseAndMouseCursorPosition(true);
                    }
                    if self.shared.hidden.swap(false, Ordering::AcqRel) {
                        let _ = CGDisplayShowCursor(kCGNullDirectDisplay);
                    }
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
        if (0..=4).any(|b| CGEventSource::button_state(SESSION, CGMouseButton(b))) {
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
        // Refresh geometry at activation, rather than assuming the portal's display is unchanged.
        let display = Display::read(portal.display.id)?;
        let current = CGEvent::new(None)
            .ok_or_else(|| PlatformError::Backend("read frozen cursor location".into()))?;
        let location = CGEvent::location(Some(&current));
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
        self.display = Some(display);
        self.release_edges(clock::now());
        self.shared
            .active
            .compare_exchange(0, token << 2 | PENDING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| PlatformError::Backend("capture recovery in progress".into()))?;
        self.shared.capturing.store(true, Ordering::Release);
        if !request.valid(&self.shared) || !self.shared.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        cg_result(
            CGAssociateMouseAndMouseCursorPosition(false),
            "detach cursor",
        )?;
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
        cg_result(CGDisplayHideCursor(kCGNullDirectDisplay), "hide cursor")?;
        self.shared.hidden.store(true, Ordering::Release);
        if !request.valid(&self.shared)
            || self.shared.active.load(Ordering::Acquire) != token << 2 | PENDING
        {
            let _ = self.shared.finish(EndReason::Lost, None);
            return Err(PlatformError::Timeout);
        }
        // Public hiding can succeed without hiding a background app's cursor (WP-1.19).
        // The disassociated cursor at the edge is the specified MVP fallback.
        Ok(CaptureStart {
            held_keys,
            lock_keys: self.lock_keys,
        })
    }

    fn commands(&mut self) {
        if self.shared.stop.load(Ordering::Acquire) {
            self.stop_when_released();
            return;
        }
        self.sync_portals();
        while let Ok(command) = self.commands.try_recv() {
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
                        match self.shared.output.send(Delivery::Subscribe(
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
                    self.begin(id, portal, token, request, reply)
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
        self.sync_portals();
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
        if self.shared.capturing.load(Ordering::Acquire)
            && (!self.shared.gate.is_open()
                || self
                    .tap
                    .as_ref()
                    .is_none_or(|tap| !CGEvent::tap_is_enabled(tap)))
        {
            let _ = self.shared.finish(EndReason::Lost, None);
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
            for (number, token) in self.suppressed_buttons.iter_mut().enumerate() {
                if *token != 0
                    && !CGEventSource::button_state(SESSION, CGMouseButton(number as u32))
                {
                    *token = 0;
                }
            }
        }
        if self.shared.stop.load(Ordering::Acquire) {
            // Dropping ends capture immediately, but the tap remains for suppressed ups.
            self.stop_when_released();
        }
    }

    fn stop_when_released(&self) {
        if self.suppressed_keys.iter().all(|token| *token == 0)
            && self.suppressed_buttons.iter().all(|token| *token == 0)
            && let Some(run_loop) = CFRunLoop::current()
        {
            run_loop.stop();
        }
    }

    /// Returns true only when the OS should receive this event.
    fn event(&mut self, kind: CGEventType, event: &CGEvent) -> bool {
        if matches!(
            kind,
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
        ) {
            if let Some(tap) = &self.tap {
                CGEvent::tap_enable(tap, true);
            }
            if self.shared.capturing.load(Ordering::Acquire) {
                let _ = self.shared.finish(EndReason::Lost, None);
            }
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
                }
            } else if self.subscribed {
                self.sync_portals();
                let location = CGEvent::location(Some(event));
                let mut pressed = HashSet::new();
                for portal in self.portals.iter() {
                    if let Some(position) = portal_hit(*portal, location, dx, dy) {
                        pressed.insert(portal.portal.id);
                        self.shared.event(
                            0,
                            CaptureEvent::EdgePressed {
                                portal: portal.portal.id,
                                position,
                                at,
                            },
                        );
                    }
                }
                for portal in self.pressed.difference(&pressed) {
                    self.shared.event(
                        0,
                        CaptureEvent::EdgeReleased {
                            portal: *portal,
                            at,
                        },
                    );
                }
                self.pressed = pressed;
            }
            return !capturing;
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
                return false;
            }
            if capturing {
                if down {
                    self.suppressed_keys[index] = token;
                }
                if !repeat && let Some(usage) = macos_to_hid(keycode) {
                    self.shared
                        .event(token, CaptureEvent::Key { usage, down, at });
                }
                return false;
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
            if self.suppressed_buttons[index] != 0 {
                if button_up {
                    let pressed_in = self.suppressed_buttons[index];
                    self.suppressed_buttons[index] = 0;
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
                return false;
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
                return false;
            }
            return true;
        }
        if kind == CGEventType::ScrollWheel
            && capturing
            && let Some(display) = self.display
        {
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
                        display.scale,
                        integer(CGEventField::ScrollWheelEventScrollPhase),
                        integer(CGEventField::ScrollWheelEventMomentumPhase),
                    ),
                    at,
                },
            );
        }
        !capturing
    }
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: the boxed context and borrowed event live through this callback; the source
    // runs only on its owning thread, and callbacks never recursively run the run loop.
    let state = unsafe { &mut *info.cast::<TapState>() };
    // SAFETY: CoreGraphics supplies a valid event for the callback's duration.
    if state.event(kind, unsafe { event.as_ref() }) {
        event.as_ptr()
    } else {
        ptr::null_mut()
    }
}

unsafe extern "C-unwind" fn command_callback(info: *mut c_void) {
    // SAFETY: same boxed context lifetime and single-thread callback ownership as tap_callback.
    unsafe { &mut *info.cast::<TapState>() }.commands();
}

unsafe extern "C-unwind" fn timer_callback(_timer: *mut CFRunLoopTimer, info: *mut c_void) {
    // SAFETY: timer is removed/invalidated before its boxed, thread-owned context is dropped.
    unsafe { &mut *info.cast::<TapState>() }.poll();
}

fn run_tap(
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    ready: Sender<Result<Arc<Wake>, PlatformError>>,
) {
    let mut state = Box::new(TapState {
        shared: shared.clone(),
        commands,
        tap: None,
        portals: Arc::new(Vec::new()),
        pressed: HashSet::new(),
        subscribed: false,
        monitor: false,
        last_activity: None,
        blinded: secure_input(),
        lock_keys: locks(CGEventSource::flags_state(SESSION)),
        display: None,
        local_keys: [false; 128],
        suppressed_keys: [0; 128],
        suppressed_buttons: [0; 256],
    });
    let info = ptr::from_mut(&mut *state).cast::<c_void>();
    let setup = || -> Result<_, PlatformError> {
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
        // SAFETY: callback only borrows the stable Box above; its port/source are invalidated
        // before the box is dropped. This is the one public, suppressing session tap.
        let tap = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mask,
                Some(tap_callback),
                info,
            )
        }
        .ok_or_else(|| {
            check_permissions()
                .err()
                .unwrap_or_else(|| PlatformError::Backend("CGEventTapCreate failed".into()))
        })?;
        let run_loop = CFRunLoop::current()
            .ok_or_else(|| PlatformError::Backend("no capture run loop".into()))?;
        let tap_source = CFMachPort::new_run_loop_source(None, Some(&tap), 0)
            .ok_or_else(|| PlatformError::Backend("create tap run-loop source".into()))?;
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
                CFAbsoluteTimeGetCurrent() + 0.25,
                0.25,
                0,
                0,
                Some(timer_callback),
                &mut context,
            )
        }
        .ok_or_else(|| PlatformError::Backend("create Secure Input timer".into()))?;
        Ok((tap, run_loop, tap_source, command_source, timer))
    };
    let (tap, run_loop, tap_source, command_source, timer) = match setup() {
        Ok(resources) => resources,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    state.tap = Some(tap);
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
    let _ = shared.finish(EndReason::Lost, None);
    timer.invalidate();
    run_loop.remove_timer(Some(&timer), mode);
    run_loop.remove_source(Some(&command_source), mode);
    run_loop.remove_source(Some(&tap_source), mode);
    command_source.invalidate();
    tap_source.invalidate();
    if let Some(tap) = &state.tap {
        CGEvent::tap_enable(tap, false);
        tap.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::CGSize;

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

    #[test]
    fn flags_changed_modifier_down_up() {
        for (keycodes, mask) in [
            (&[0x38, 0x3c][..], CGEventFlags::MaskShift),
            (&[0x3b, 0x3e][..], CGEventFlags::MaskControl),
            (&[0x3a, 0x3d][..], CGEventFlags::MaskAlternate),
            (&[0x37, 0x36][..], CGEventFlags::MaskCommand),
            (&[0x3f][..], CGEventFlags::MaskSecondaryFn),
        ] {
            for &code in keycodes {
                assert_eq!(modifier_down(code, mask), Some(true));
                assert_eq!(modifier_down(code, CGEventFlags::empty()), Some(false));
            }
        }
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
        let discrete = scroll(false, 1.0, -2.0, 2.0, 0, 0);
        assert_eq!((discrete.v120_x, discrete.v120_y), (120, -240));
        assert_eq!(discrete.pixels, None);
        assert_eq!(discrete.phase, ScrollPhase::Discrete);
        assert_eq!(
            scroll(true, 0.0, 0.0, 2.0, 0, 0).phase,
            ScrollPhase::Discrete
        );
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
            let converted = scroll(true, -3.0, 4.0, 2.0, phase, momentum);
            assert_eq!(converted.pixels, Some(VectorLogical::new(-6.0, 8.0)));
            assert_eq!((converted.v120_x, converted.v120_y), (0, 0));
            assert_eq!(converted.phase, expected);
            assert_eq!(scroll(true, 0.0, 0.0, 2.0, phase, momentum).phase, expected);
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
