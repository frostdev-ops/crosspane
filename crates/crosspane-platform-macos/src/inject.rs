//! Physical input through one private CGEventSource. CGEventPost submits events; it cannot tell
//! whether an application received them. The target's NSEvent settings drive synthesized repeat.
//! Scroll signs follow WP-1.17: positive x is scroll right and positive y is scroll up.
//! Quartz axis 2 has the opposite sign, so negate x only at output, after carrying remainders
//! in Crosspane's domain. Natural scrolling is already applied by capture. Pixels are target
//! logical points, not device pixels.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use crosspane_platform::{IoGate, KeyInjector, Permission, PlatformError, PointerInjector};
use crosspane_types::geom::PointDevice;
use crosspane_types::hid::{HidUsage, MouseButton, hid_to_macos};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use crosspane_types::time::MonoTime;
use objc2_app_kit::NSEvent;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect};
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsActive, CGDisplayMode, CGEvent,
    CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMouseButton, CGPreflightPostEventAccess, CGScrollEventUnit,
};

use crate::{clock, main_thread::on_main};

const TAG: i64 = 0x0043_5049_4E4A;
const BUDGET: Duration = Duration::from_millis(50);
const WATCHDOG: Duration = Duration::from_millis(5);
const DROP_BUDGET: Duration = Duration::from_millis(500);
const PERMISSION_CACHE: Duration = Duration::from_secs(1);
const CAPS_LOCK: HidUsage = HidUsage::keyboard(0x39);

// Apple's public HIToolbox/CarbonEventsCore.h declares this Boolean-returning function.
// The injection mutex serializes calls, including those on the repeat worker.
// SAFETY: the declaration matches Apple's public header (Boolean is an unsigned byte).
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

fn secure_input() -> bool {
    // Carbon's public header labels this query not thread safe. Serialize all injector pairs;
    // contention or poisoning cannot prove Secure Input is off, so fail closed.
    static QUERY: Mutex<()> = Mutex::new(());
    let Ok(_query) = QUERY.try_lock() else {
        return true;
    };
    // SAFETY: public, argument-free status query; never enables or disables Secure Event Input.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

fn permission() -> Result<(), PlatformError> {
    if CGPreflightPostEventAccess() {
        Ok(())
    } else {
        Err(PlatformError::PermissionDenied(Permission::Accessibility))
    }
}

fn backend(message: &'static str) -> PlatformError {
    PlatformError::Backend(message.into())
}

#[derive(Debug)]
struct Source(CFRetained<CGEventSource>);

// SAFETY: CGEventSource has no run-loop/thread affinity. The retained source is never exposed;
// creation, event construction, posting and destruction are serialized by Shared::state.
unsafe impl Send for Source {}

#[derive(Debug)]
struct Shared {
    gate: Arc<IoGate>,
    state: Mutex<State>,
    changed: Condvar,
    handles: AtomicU8,
    repeat_key: AtomicU32,
}

impl Shared {
    fn lock(&self, deadline: Instant) -> Result<MutexGuard<'_, State>, PlatformError> {
        self.lock_inner(deadline, false)
    }

    fn release_lock(&self, deadline: Instant) -> Result<MutexGuard<'_, State>, PlatformError> {
        self.lock_inner(deadline, true)
    }

    fn lock_inner(
        &self,
        deadline: Instant,
        releasing: bool,
    ) -> Result<MutexGuard<'_, State>, PlatformError> {
        loop {
            match self.state.try_lock() {
                Ok(state) => return Ok(state),
                Err(TryLockError::Poisoned(error)) if releasing => return Ok(error.into_inner()),
                Err(TryLockError::Poisoned(_)) => return Err(backend("injection state poisoned")),
                Err(TryLockError::WouldBlock) if Instant::now() >= deadline => {
                    return Err(PlatformError::Timeout);
                }
                Err(TryLockError::WouldBlock) => std::thread::yield_now(),
            }
        }
    }

    fn release_on_drop(&self, key_handle: bool) {
        let deadline = Instant::now() + DROP_BUDGET;
        while Instant::now() < deadline {
            {
                let Ok(mut state) = self.release_lock(deadline) else {
                    break;
                };
                // Notify under the mutex as well, so an idle worker cannot miss the wake-up.
                self.changed.notify_one();
                let done = if key_handle {
                    state.stop_repeat(self);
                    let keys: Vec<_> = state.keys.iter().copied().collect();
                    let _ = release_keys(&mut state, &keys, self, deadline);
                    state.keys.is_empty() && state.caps_up_owed.is_none()
                } else {
                    let buttons: Vec<_> = state.buttons.iter().copied().collect();
                    let _ = release_buttons(&mut state, &buttons, self, deadline);
                    state.buttons.is_empty() && state.gesture.is_none()
                };
                if done {
                    break;
                }
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(WATCHDOG),
            );
        }
        self.changed.notify_one();
    }
}

#[derive(Debug)]
struct State {
    source: Source,
    keys: BTreeSet<HidUsage>,
    buttons: BTreeSet<MouseButton>,
    clicks: [Click; 32],
    click_interval: Duration,
    last_point: Option<(CGPoint, MonoTime)>,
    permission_checked: Instant,
    post_access: bool,
    line_remainder: [i64; 2],
    pixel_remainder: [f64; 2],
    gesture: Option<Gesture>,
    caps_up_owed: Option<bool>,
    caps_key_flags: Option<bool>,
    repeat: RepeatSchedule,
}

#[derive(Clone, Copy, Debug)]
enum Gesture {
    MayBegin,
    Touch,
    Momentum,
}

impl Gesture {
    fn end_phase(self) -> ScrollPhase {
        match self {
            Self::MayBegin => ScrollPhase::Cancelled,
            Self::Touch => ScrollPhase::Ended,
            Self::Momentum => ScrollPhase::MomentumEnded,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Submission {
    Press,
    Release,
    Repeat(HidUsage),
}

fn repeat_key(usage: HidUsage) -> u32 {
    (u32::from(usage.page) << 16) | u32::from(usage.id)
}

impl State {
    fn stop_repeat(&mut self, shared: &Shared) {
        shared.repeat_key.store(0, Ordering::Release);
        self.repeat.stop();
    }

    fn sync_repeat(&mut self, shared: &Shared) {
        if self.repeat.pending.is_some_and(|(usage, _)| {
            shared.repeat_key.load(Ordering::Acquire) != repeat_key(usage)
        }) {
            self.repeat.stop();
        }
    }

    fn flags(&self) -> CGEventFlags {
        modifier_flags(&self.keys) | caps_flags(caps_lock())
    }

    fn post(
        &mut self,
        event: &CGEvent,
        flags: CGEventFlags,
        submission: Submission,
        shared: &Shared,
    ) -> Result<(), PlatformError> {
        CGEvent::set_flags(Some(event), flags);
        CGEvent::set_integer_value_field(Some(event), CGEventField::EventSourceUserData, TAG);
        self.check_submission(submission, shared)?;
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
        Ok(())
    }

    fn check_submission(
        &mut self,
        submission: Submission,
        shared: &Shared,
    ) -> Result<(), PlatformError> {
        if self.permission_checked.elapsed() >= PERMISSION_CACHE {
            self.post_access = CGPreflightPostEventAccess();
            self.permission_checked = Instant::now();
        }
        if !self.post_access {
            self.stop_repeat(shared);
            return Err(PlatformError::PermissionDenied(Permission::Accessibility));
        }
        if !matches!(submission, Submission::Release) {
            if !shared.gate.is_open() {
                self.stop_repeat(shared);
                let _ = self.end_gesture(shared);
                return Err(PlatformError::Locked);
            }
            if secure_input() {
                self.stop_repeat(shared);
                // A zero-displacement gesture end is a release, and remains owed if it fails.
                let _ = self.end_gesture(shared);
                return Err(PlatformError::SecureInput);
            }
            // Last check before the OS submission. No application receipt is implied by Ok.
            if !shared.gate.is_open() {
                self.stop_repeat(shared);
                let _ = self.end_gesture(shared);
                return Err(PlatformError::Locked);
            }
            if let Submission::Repeat(usage) = submission
                && (shared.handles.load(Ordering::Acquire) & 1 == 0
                    || shared.repeat_key.load(Ordering::Acquire) != repeat_key(usage))
            {
                self.repeat.stop();
                return Err(PlatformError::Locked);
            }
        }
        Ok(())
    }

    fn key(&mut self, usage: HidUsage, down: bool, shared: &Shared) -> Result<(), PlatformError> {
        self.sync_repeat(shared);
        // Stop a released repeat before any fallible allocation or permission check.
        if !down {
            self.repeat.release(usage);
        } else if !usage.is_modifier() {
            self.repeat.stop();
        }
        let code = hid_to_macos(usage).ok_or(PlatformError::Unsupported("unmapped key"))?;
        let event = CGEvent::new_keyboard_event(Some(&self.source.0), code, down)
            .ok_or_else(|| backend("create keyboard event"))?;
        CGEvent::set_integer_value_field(Some(&event), CGEventField::KeyboardEventAutorepeat, 0);
        let mut keys = self.keys.clone();
        if down {
            keys.insert(usage);
        } else {
            keys.remove(&usage);
        }
        let mut flags = modifier_flags(&keys) | caps_flags(caps_lock());
        if usage.is_modifier() || usage == CAPS_LOCK {
            CGEvent::set_type(Some(&event), CGEventType::FlagsChanged);
            if usage == CAPS_LOCK {
                let caps = if down {
                    !caps_lock()
                } else {
                    self.caps_key_flags.unwrap_or_else(caps_lock)
                };
                flags = modifier_flags(&keys) | caps_flags(caps);
            }
        }
        self.post(
            &event,
            flags,
            if down {
                Submission::Press
            } else {
                Submission::Release
            },
            shared,
        )?;
        self.keys = keys;
        if usage == CAPS_LOCK {
            self.caps_key_flags = down.then_some(flags.contains(CGEventFlags::MaskAlphaShift));
        }
        if down {
            self.repeat.press(usage, clock::now());
            if !usage.is_modifier() {
                shared.repeat_key.store(
                    self.repeat
                        .pending
                        .map(|(key, _)| repeat_key(key))
                        .unwrap_or(0),
                    Ordering::Release,
                );
            }
            shared.changed.notify_one();
        }
        Ok(())
    }

    fn button(
        &mut self,
        button: MouseButton,
        down: bool,
        shared: &Shared,
    ) -> Result<(), PlatformError> {
        let number = button_number(button)?;
        let position = self.cursor_position()?;
        let mut click = self.clicks[usize::from(number)];
        if down {
            click.down(clock::now(), position, self.click_interval);
        }
        let event = CGEvent::new_mouse_event(
            Some(&self.source.0),
            button_event(number, down),
            position,
            CGMouseButton(u32::from(number)),
        )
        .ok_or_else(|| backend("create button event"))?;
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::MouseEventClickState,
            click.count,
        );
        let at = clock::now();
        self.post(
            &event,
            self.flags(),
            if down {
                Submission::Press
            } else {
                Submission::Release
            },
            shared,
        )?;
        self.last_point = Some((position, at));
        self.clicks[usize::from(number)] = click;
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        Ok(())
    }

    fn cursor_position(&self) -> Result<CGPoint, PlatformError> {
        let now = clock::now();
        // CGEventCreate(NULL) has a zero timestamp on this Mac. Use the public elapsed-time
        // query for the most recent OS motion instead of guessing ticks versus nanoseconds.
        let age = [
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
        ]
        .into_iter()
        .map(|kind| {
            CGEventSource::seconds_since_last_event_type(
                CGEventSourceStateID::CombinedSessionState,
                kind,
            )
        })
        .fold(f64::INFINITY, f64::min);
        Ok(latest_point(self.last_point, cursor()?, now, age))
    }

    fn end_gesture(&mut self, shared: &Shared) -> Result<(), PlatformError> {
        let Some(gesture) = self.gesture else {
            return Ok(());
        };
        let event = CGEvent::new_scroll_wheel_event2(
            Some(&self.source.0),
            CGScrollEventUnit::Pixel,
            2,
            0,
            0,
            0,
        )
        .ok_or_else(|| backend("create scroll end"))?;
        scroll_fields(&event, gesture.end_phase());
        self.post(&event, self.flags(), Submission::Release, shared)?;
        self.gesture = None;
        self.pixel_remainder = [0.0; 2];
        Ok(())
    }

    fn finish_caps_tap(&mut self, shared: &Shared) -> Result<(), PlatformError> {
        let Some(caps) = self.caps_up_owed else {
            return Ok(());
        };
        let up = CGEvent::new_keyboard_event(Some(&self.source.0), 0x39, false)
            .ok_or_else(|| backend("create caps lock up"))?;
        CGEvent::set_type(Some(&up), CGEventType::FlagsChanged);
        self.post(
            &up,
            modifier_flags(&self.keys) | caps_flags(caps),
            Submission::Release,
            shared,
        )?;
        self.caps_up_owed = None;
        Ok(())
    }
}

/// Key command handle. Its repeat worker shares the pointer's source and modifier ledger.
#[derive(Debug)]
pub struct MacKeyInjector {
    shared: Arc<Shared>,
}

/// Pointer command handle. Held key modifiers apply to motion, buttons and scrolling.
#[derive(Debug)]
pub struct MacPointerInjector {
    shared: Arc<Shared>,
}

/// Fails with PermissionDenied(Accessibility) unless CGPreflightPostEventAccess().
pub fn injectors(gate: Arc<IoGate>) -> Result<(MacKeyInjector, MacPointerInjector), PlatformError> {
    permission()?;
    let (click_interval, delay, interval) = match on_main(BUDGET, |_| {
        (
            NSEvent::doubleClickInterval(),
            NSEvent::keyRepeatDelay(),
            NSEvent::keyRepeatInterval(),
        )
    }) {
        Ok(settings) => settings,
        Err(PlatformError::Timeout) => (0.5, 0.5, 0.083),
        Err(error) => return Err(error),
    };
    let source = CGEventSource::new(CGEventSourceStateID::Private)
        .ok_or_else(|| backend("create injection source"))?;
    CGEventSource::set_user_data(Some(&source), TAG);
    let shared = Arc::new(Shared {
        gate,
        state: Mutex::new(State {
            source: Source(source),
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            clicks: [Click::default(); 32],
            click_interval: duration(click_interval, Duration::from_millis(500)),
            last_point: None,
            permission_checked: Instant::now(),
            post_access: true,
            line_remainder: [0; 2],
            pixel_remainder: [0.0; 2],
            gesture: None,
            caps_up_owed: None,
            caps_key_flags: None,
            repeat: RepeatSchedule {
                delay: duration(delay, Duration::from_millis(500)),
                interval: duration(interval, Duration::from_millis(83)),
                pending: None,
            },
        }),
        changed: Condvar::new(),
        handles: AtomicU8::new(3),
        repeat_key: AtomicU32::new(0),
    });
    let worker = shared.clone();
    std::thread::Builder::new()
        .name("mac-input-repeat".into())
        .spawn(move || repeat_worker(&worker))
        .map_err(|_| backend("spawn input repeat worker"))?;
    Ok((
        MacKeyInjector {
            shared: shared.clone(),
        },
        MacPointerInjector { shared },
    ))
}

fn duration(seconds: f64, default: Duration) -> Duration {
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|value| !value.is_zero())
        .unwrap_or(default)
}

fn repeat_worker(shared: &Shared) {
    let Ok(mut state) = shared.state.lock() else {
        return;
    };
    while shared.handles.load(Ordering::Acquire) != 0 {
        state.sync_repeat(shared);
        if shared.handles.load(Ordering::Acquire) & 1 == 0 {
            state.stop_repeat(shared);
        }
        if state.repeat.pending.is_none() && state.gesture.is_none() {
            // The timeout also guarantees exit if a drop times out acquiring the mutex.
            let Ok((next, _)) = shared.changed.wait_timeout(state, DROP_BUDGET) else {
                return;
            };
            state = next;
            continue;
        }
        if !shared.gate.is_open() || secure_input() {
            state.stop_repeat(shared);
            let _ = state.end_gesture(shared);
        } else if let Some(usage) = state.repeat.due(clock::now) {
            let event = hid_to_macos(usage)
                .and_then(|code| CGEvent::new_keyboard_event(Some(&state.source.0), code, true));
            if let Some(event) = event {
                CGEvent::set_integer_value_field(
                    Some(&event),
                    CGEventField::KeyboardEventAutorepeat,
                    1,
                );
                let flags = state.flags();
                // Dropping the key handle forbids any new repeat submission.
                if shared.handles.load(Ordering::Acquire) & 1 == 0
                    || state
                        .post(&event, flags, Submission::Repeat(usage), shared)
                        .is_err()
                {
                    state.stop_repeat(shared);
                }
            } else {
                state.stop_repeat(shared);
            }
        }
        let wait = state
            .repeat
            .pending
            .map(|(_, at)| at.saturating_duration_since(clock::now()).min(WATCHDOG))
            .unwrap_or(WATCHDOG);
        let Ok((next, _)) = shared.changed.wait_timeout(state, wait) else {
            return;
        };
        state = next;
    }
}

impl KeyInjector for MacKeyInjector {
    fn key(&mut self, usage: HidUsage, down: bool) -> Result<(), PlatformError> {
        // Cancellation must precede even a failed/timed-out mutex acquisition.
        if down && !usage.is_modifier() {
            self.shared.repeat_key.store(0, Ordering::Release);
        } else if !down {
            let _ = self.shared.repeat_key.compare_exchange(
                repeat_key(usage),
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        self.shared.changed.notify_one();
        let deadline = Instant::now() + BUDGET;
        let mut state = if down {
            self.shared.lock(deadline)?
        } else {
            self.shared.release_lock(deadline)?
        };
        state.key(usage, down, &self.shared)
    }

    fn lock_keys(&self) -> Result<LockKeys, PlatformError> {
        Ok(LockKeys {
            caps_lock: Some(caps_lock()),
            num_lock: None,
            scroll_lock: None,
        })
    }

    fn set_lock_keys(&mut self, wanted: LockKeys) -> Result<(), PlatformError> {
        let Some(wanted) = wanted.caps_lock else {
            return Ok(());
        };
        let deadline = Instant::now() + BUDGET;
        let mut state = self.shared.lock(deadline)?;
        state.finish_caps_tap(&self.shared)?;
        if caps_lock() == wanted {
            return Ok(());
        }
        state.stop_repeat(&self.shared);
        let flags = modifier_flags(&state.keys) | caps_flags(wanted);
        let down = CGEvent::new_keyboard_event(Some(&state.source.0), 0x39, true)
            .ok_or_else(|| backend("create caps lock down"))?;
        let up = CGEvent::new_keyboard_event(Some(&state.source.0), 0x39, false)
            .ok_or_else(|| backend("create caps lock up"))?;
        CGEvent::set_type(Some(&down), CGEventType::FlagsChanged);
        CGEvent::set_type(Some(&up), CGEventType::FlagsChanged);
        state.post(&down, flags, Submission::Press, &self.shared)?;
        if state.keys.contains(&CAPS_LOCK) {
            state.caps_key_flags = Some(wanted);
        }
        // Keep tap recovery separate from the physical-key ledger: lock sync changes no ownership.
        state.caps_up_owed = Some(wanted);
        state.post(&up, flags, Submission::Release, &self.shared)?;
        state.caps_up_owed = None;
        // Give WindowServer a full verification window after the completed tap.
        let deadline = Instant::now() + BUDGET;
        drop(state);
        loop {
            if caps_lock() == wanted {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PlatformError::Unsupported("caps lock"));
            }
            std::thread::sleep(remaining.min(Duration::from_millis(1)));
        }
    }

    fn release_all(&mut self) -> Result<(), PlatformError> {
        self.shared.repeat_key.store(0, Ordering::Release);
        self.shared.changed.notify_one();
        let deadline = Instant::now() + BUDGET;
        let mut state = self.shared.release_lock(deadline)?;
        state.repeat.stop();
        self.shared.changed.notify_one();
        let keys: Vec<_> = state.keys.iter().copied().collect();
        release_keys(&mut state, &keys, &self.shared, deadline)
    }

    fn recover_keys(&mut self, keys: &[HidUsage]) -> Result<(), PlatformError> {
        self.shared.repeat_key.store(0, Ordering::Release);
        self.shared.changed.notify_one();
        let deadline = Instant::now() + BUDGET;
        let mut state = self.shared.release_lock(deadline)?;
        state.repeat.stop();
        self.shared.changed.notify_one();
        release_keys(&mut state, keys, &self.shared, deadline)
    }
}

fn release_keys(
    state: &mut State,
    keys: &[HidUsage],
    shared: &Shared,
    deadline: Instant,
) -> Result<(), PlatformError> {
    let mut error = state.finish_caps_tap(shared).err();
    for &key in keys {
        if Instant::now() >= deadline {
            return Err(error.unwrap_or(PlatformError::Timeout));
        }
        if let Err(failure) = state.key(key, false, shared) {
            error.get_or_insert(failure);
        }
    }
    error.map_or(Ok(()), Err)
}

impl PointerInjector for MacPointerInjector {
    fn move_to(&mut self, display: DisplayId, position: PointDevice) -> Result<(), PlatformError> {
        if !position.x.is_finite() || !position.y.is_finite() {
            return Err(PlatformError::Unsupported("non-finite pointer position"));
        }
        let mut state = self.shared.lock(Instant::now() + BUDGET)?;
        if !CGDisplayIsActive(display.0) {
            return Err(PlatformError::NotFound);
        }
        let bounds = CGDisplayBounds(display.0);
        let mode = CGDisplayCopyDisplayMode(display.0).ok_or(PlatformError::NotFound)?;
        let width = CGDisplayMode::width(Some(&mode)) as f64;
        let scale = CGDisplayMode::pixel_width(Some(&mode)) as f64 / width;
        if !scale.is_finite() || scale <= 0.0 {
            return Err(PlatformError::Unsupported("display scale"));
        }
        if !bounds.size.width.is_finite()
            || !bounds.size.height.is_finite()
            || bounds.size.width <= 0.0
            || bounds.size.height <= 0.0
        {
            return Err(PlatformError::Unsupported("display bounds"));
        }
        let point = display_point(bounds, position, scale);
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(PlatformError::Unsupported("non-finite display point"));
        }
        let button = state.buttons.first().copied();
        let number = button.map(button_number).transpose()?.unwrap_or(0);
        let kind = match button {
            None => CGEventType::MouseMoved,
            Some(MouseButton::PRIMARY) => CGEventType::LeftMouseDragged,
            Some(MouseButton::SECONDARY) => CGEventType::RightMouseDragged,
            Some(_) => CGEventType::OtherMouseDragged,
        };
        let event = CGEvent::new_mouse_event(
            Some(&state.source.0),
            kind,
            point,
            CGMouseButton(u32::from(number)),
        )
        .ok_or_else(|| backend("create pointer motion"))?;
        let previous = match state.last_point {
            Some((point, _)) => point,
            None => cursor()?,
        };
        let [dx, dy] = mouse_delta(point, previous);
        for (field, value) in [
            (CGEventField::MouseEventDeltaX, dx),
            (CGEventField::MouseEventDeltaY, dy),
        ] {
            CGEvent::set_integer_value_field(Some(&event), field, value);
        }
        if button.is_some() {
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventClickState,
                state.clicks[usize::from(number)].count,
            );
        }
        let flags = state.flags();
        let at = clock::now();
        state.post(&event, flags, Submission::Press, &self.shared)?;
        state.last_point = Some((point, at));
        Ok(())
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        let deadline = Instant::now() + BUDGET;
        let mut state = if down {
            self.shared.lock(deadline)?
        } else {
            self.shared.release_lock(deadline)?
        };
        state.button(button, down, &self.shared)
    }

    fn scroll(&mut self, delta: ScrollDelta) -> Result<(), PlatformError> {
        let mut state = self.shared.lock(Instant::now() + BUDGET)?;
        let mut remainder = state.line_remainder;
        let mut pixel_remainder = state.pixel_remainder;
        let (units, x, y) = if let Some(pixels) = delta.pixels {
            let (x, rest_x) = pixel_delta(pixels.x, pixel_remainder[0])?;
            let (y, rest_y) = pixel_delta(pixels.y, pixel_remainder[1])?;
            pixel_remainder = [rest_x, rest_y];
            (CGScrollEventUnit::Pixel, x, y)
        } else {
            state.end_gesture(&self.shared)?;
            pixel_remainder = [0.0; 2];
            let (x, rest_x) = lines(delta.v120_x, remainder[0]);
            let (y, rest_y) = lines(delta.v120_y, remainder[1]);
            remainder = [rest_x, rest_y];
            (CGScrollEventUnit::Line, x, y)
        };
        if delta.pixels.is_none() && x == 0 && y == 0 {
            // Keep the residual in Crosspane units, without submitting a zero line event.
            state.check_submission(Submission::Press, &self.shared)?;
            state.line_remainder = remainder;
            state.pixel_remainder = pixel_remainder;
            return Ok(());
        }
        let [axis1, axis2] = quartz_scroll(x, y);
        let event =
            CGEvent::new_scroll_wheel_event2(Some(&state.source.0), units, 2, axis1, axis2, 0)
                .ok_or_else(|| backend("create scroll event"))?;
        if delta.pixels.is_some() {
            scroll_fields(&event, delta.phase);
        }
        let flags = state.flags();
        state.post(&event, flags, Submission::Press, &self.shared)?;
        state.line_remainder = remainder;
        state.pixel_remainder = pixel_remainder;
        state.gesture = if delta.pixels.is_some() {
            match delta.phase {
                ScrollPhase::MayBegin => Some(Gesture::MayBegin),
                ScrollPhase::Began | ScrollPhase::Changed => Some(Gesture::Touch),
                ScrollPhase::MomentumBegan | ScrollPhase::MomentumChanged => {
                    Some(Gesture::Momentum)
                }
                _ => None,
            }
        } else {
            None
        };
        if state.gesture.is_none() {
            state.pixel_remainder = [0.0; 2];
        }
        self.shared.changed.notify_one();
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), PlatformError> {
        let deadline = Instant::now() + BUDGET;
        let mut state = self.shared.release_lock(deadline)?;
        self.shared.changed.notify_one();
        let buttons: Vec<_> = state.buttons.iter().copied().collect();
        release_buttons(&mut state, &buttons, &self.shared, deadline)
    }

    fn recover_buttons(&mut self, buttons: &[MouseButton]) -> Result<(), PlatformError> {
        let deadline = Instant::now() + BUDGET;
        let mut state = self.shared.release_lock(deadline)?;
        release_buttons(&mut state, buttons, &self.shared, deadline)
    }
}

fn release_buttons(
    state: &mut State,
    buttons: &[MouseButton],
    shared: &Shared,
    deadline: Instant,
) -> Result<(), PlatformError> {
    let mut error = state.end_gesture(shared).err();
    state.line_remainder = [0; 2];
    state.pixel_remainder = [0.0; 2];
    for &button in buttons {
        if Instant::now() >= deadline {
            return Err(error.unwrap_or(PlatformError::Timeout));
        }
        if let Err(failure) = state.button(button, false, shared) {
            error.get_or_insert(failure);
        }
    }
    error.map_or(Ok(()), Err)
}

impl Drop for MacKeyInjector {
    fn drop(&mut self) {
        self.shared.repeat_key.store(0, Ordering::Release);
        self.shared.handles.fetch_and(!1, Ordering::AcqRel);
        self.shared.changed.notify_one();
        self.shared.release_on_drop(true);
    }
}

impl Drop for MacPointerInjector {
    fn drop(&mut self) {
        self.shared.handles.fetch_and(!2, Ordering::AcqRel);
        self.shared.changed.notify_one();
        self.shared.release_on_drop(false);
    }
}

fn caps_lock() -> bool {
    CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState)
        .contains(CGEventFlags::MaskAlphaShift)
}

fn caps_flags(caps: bool) -> CGEventFlags {
    if caps {
        CGEventFlags::MaskAlphaShift
    } else {
        CGEventFlags::empty()
    }
}

fn modifier_flags(keys: &BTreeSet<HidUsage>) -> CGEventFlags {
    keys.iter().fold(CGEventFlags::empty(), |flags, usage| {
        flags
            | if usage.page == HidUsage::PAGE_KEYBOARD {
                match usage.id {
                    // Public device-dependent masks from IOKit/hidsystem/IOLLEvent.h.
                    0xE0 => CGEventFlags::MaskControl | CGEventFlags(0x1),
                    0xE1 => CGEventFlags::MaskShift | CGEventFlags(0x2),
                    0xE2 => CGEventFlags::MaskAlternate | CGEventFlags(0x20),
                    0xE3 => CGEventFlags::MaskCommand | CGEventFlags(0x8),
                    0xE4 => CGEventFlags::MaskControl | CGEventFlags(0x2000),
                    0xE5 => CGEventFlags::MaskShift | CGEventFlags(0x4),
                    0xE6 => CGEventFlags::MaskAlternate | CGEventFlags(0x40),
                    0xE7 => CGEventFlags::MaskCommand | CGEventFlags(0x10),
                    _ => CGEventFlags::empty(),
                }
            } else {
                CGEventFlags::empty()
            }
    })
}

fn cursor() -> Result<CGPoint, PlatformError> {
    let event = CGEvent::new(None).ok_or_else(|| backend("read cursor location"))?;
    Ok(CGEvent::location(Some(&event)))
}

fn latest_point(
    posted: Option<(CGPoint, MonoTime)>,
    observed: CGPoint,
    now: MonoTime,
    observed_age: f64,
) -> CGPoint {
    if let Some((point, at)) = posted
        && observed_age >= 0.0
        && observed_age > now.saturating_duration_since(at).as_secs_f64()
    {
        point
    } else {
        observed
    }
}

fn mouse_delta(point: CGPoint, previous: CGPoint) -> [i64; 2] {
    // Quartz's delta fields are integer logical points, so round fractional positions.
    [
        (point.x - previous.x).round() as i64,
        (point.y - previous.y).round() as i64,
    ]
}

fn display_point(bounds: CGRect, position: PointDevice, scale: f64) -> CGPoint {
    // Clamp to the last device pixel; the display's right/bottom edges are exclusive.
    let x = position
        .x
        .clamp(0.0, (bounds.size.width * scale - 1.0).max(0.0));
    let y = position
        .y
        .clamp(0.0, (bounds.size.height * scale - 1.0).max(0.0));
    CGPoint::new(bounds.origin.x + x / scale, bounds.origin.y + y / scale)
}

fn button_number(button: MouseButton) -> Result<u8, PlatformError> {
    match button.0 {
        1..=32 => Ok(button.0 - 1),
        _ => Err(PlatformError::Unsupported("mouse button")),
    }
}

fn button_event(number: u8, down: bool) -> CGEventType {
    match (number, down) {
        (0, true) => CGEventType::LeftMouseDown,
        (0, false) => CGEventType::LeftMouseUp,
        (1, true) => CGEventType::RightMouseDown,
        (1, false) => CGEventType::RightMouseUp,
        (_, true) => CGEventType::OtherMouseDown,
        (_, false) => CGEventType::OtherMouseUp,
    }
}

#[derive(Clone, Copy, Debug)]
struct Click {
    at: Option<MonoTime>,
    position: CGPoint,
    count: i64,
}

impl Default for Click {
    fn default() -> Self {
        Self {
            at: None,
            position: CGPoint::new(0.0, 0.0),
            count: 1,
        }
    }
}

impl Click {
    fn down(&mut self, at: MonoTime, position: CGPoint, interval: Duration) {
        let close = (position.x - self.position.x).hypot(position.y - self.position.y) <= 4.0;
        self.count = if close
            && self
                .at
                .is_some_and(|last| at >= last && at.saturating_duration_since(last) <= interval)
        {
            self.count.saturating_add(1)
        } else {
            1
        };
        self.at = Some(at);
        self.position = position;
    }
}

fn lines(v120: i32, remainder: i64) -> (i32, i64) {
    let total = i64::from(v120) + remainder;
    // The previous remainder is in -119..=119, so dividing an i32 input is always in range.
    ((total / 120) as i32, total % 120)
}

fn quartz_scroll(x: i32, y: i32) -> [i32; 2] {
    [y, x.saturating_neg()]
}

fn pixel_delta(value: f64, remainder: f64) -> Result<(i32, f64), PlatformError> {
    let total = value + remainder;
    let whole = total.trunc();
    if !whole.is_finite() || whole < f64::from(i32::MIN) || whole > f64::from(i32::MAX) {
        return Err(PlatformError::Unsupported("pixel scroll delta"));
    }
    // CGEventCreateScrollWheelEvent2 accepts integer pixels. Carry fractions through a gesture
    // instead of losing every sub-pixel step; the residual is less than one point per axis.
    Ok((whole as i32, total - whole))
}

fn scroll_fields(event: &CGEvent, phase: ScrollPhase) {
    let (touch, momentum) = match phase {
        ScrollPhase::Discrete => (0, 0),
        ScrollPhase::MayBegin => (128, 0),
        ScrollPhase::Began => (1, 0),
        ScrollPhase::Changed => (2, 0),
        ScrollPhase::Ended => (4, 0),
        ScrollPhase::Cancelled => (8, 0),
        ScrollPhase::MomentumBegan => (0, 1),
        ScrollPhase::MomentumChanged => (0, 2),
        ScrollPhase::MomentumEnded => (0, 3),
    };
    for (field, value) in [
        (CGEventField::ScrollWheelEventIsContinuous, 1),
        (CGEventField::ScrollWheelEventScrollPhase, touch),
        (CGEventField::ScrollWheelEventMomentumPhase, momentum),
    ] {
        CGEvent::set_integer_value_field(Some(event), field, value);
    }
}

#[derive(Debug)]
struct RepeatSchedule {
    delay: Duration,
    interval: Duration,
    pending: Option<(HidUsage, MonoTime)>,
}

impl RepeatSchedule {
    fn press(&mut self, usage: HidUsage, at: MonoTime) {
        if !usage.is_modifier() {
            // CapsLock is a flags-changed toggle, not a repeatable key-down.
            self.pending = (usage != CAPS_LOCK).then_some((usage, at.saturating_add(self.delay)));
        }
    }

    fn release(&mut self, usage: HidUsage) {
        if self.pending.is_some_and(|(key, _)| key == usage) {
            self.stop();
        }
    }

    // Inject the clock reading, so scheduling tests never need an OS clock or timer thread.
    fn due(&mut self, now: impl FnOnce() -> MonoTime) -> Option<HidUsage> {
        let (usage, at) = self.pending?;
        let now = now();
        if now < at {
            return None;
        }
        // No burst of missed repeats after a delayed wake: resume at the target's interval.
        self.pending = Some((usage, now.saturating_add(self.interval)));
        Some(usage)
    }

    fn stop(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::CGSize;

    #[test]
    fn held_modifiers_derive_flags() {
        let mut keys: BTreeSet<_> = (0xE0..=0xE7).map(HidUsage::keyboard).collect();
        keys.insert(HidUsage::keyboard(0x04));
        keys.insert(HidUsage { page: 1, id: 0xE1 });
        let all = CGEventFlags::MaskControl
            | CGEventFlags::MaskShift
            | CGEventFlags::MaskAlternate
            | CGEventFlags::MaskCommand
            | CGEventFlags(0x207F);
        assert_eq!(modifier_flags(&keys), all);
        keys.remove(&HidUsage::keyboard(0xE1));
        assert_eq!(modifier_flags(&keys), CGEventFlags(all.0 & !0x2));
        keys.remove(&HidUsage::keyboard(0xE5));
        assert_eq!(
            modifier_flags(&keys),
            CGEventFlags(all.0 & !(CGEventFlags::MaskShift.0 | 0x6))
        );
        assert_eq!(modifier_flags(&BTreeSet::new()), CGEventFlags::empty());
    }

    #[test]
    fn releasing_left_shift_preserves_right_shift() {
        let left = HidUsage::keyboard(0xE1);
        let right = HidUsage::keyboard(0xE5);
        let mut keys = BTreeSet::from([left, right]);
        assert_eq!(
            modifier_flags(&keys),
            CGEventFlags::MaskShift | CGEventFlags(0x6)
        );
        keys.remove(&left); // The release event uses the prospective held-key ledger.
        assert!(left.is_modifier()); // Modifier transitions are FlagsChanged, not KeyUp.
        assert_eq!(hid_to_macos(left), Some(0x38));
        assert_eq!(
            modifier_flags(&keys),
            CGEventFlags::MaskShift | CGEventFlags(0x4)
        );
        assert!(!modifier_flags(&keys).contains(CGEventFlags(0x2)));
    }

    #[test]
    fn click_state_timing_and_distance() {
        let mut click = Click::default();
        let at = |ms: u64| MonoTime::from_nanos(ms * 1_000_000);
        let interval = Duration::from_millis(500);
        click.down(at(0), CGPoint::new(0.0, 0.0), interval);
        assert_eq!(click.count, 1);
        click.down(at(500), CGPoint::new(0.0, 4.0), interval);
        assert_eq!(click.count, 2);
        click.down(at(501), CGPoint::new(3.0, 7.0), interval);
        assert_eq!(click.count, 1); // diagonal distance exceeds four points
        click.down(at(1002), CGPoint::new(3.0, 7.0), interval);
        assert_eq!(click.count, 1);
        click.down(at(1003), CGPoint::new(3.0, 7.0), interval);
        assert_eq!(click.count, 2);
        assert_eq!(Click::default().count, 1); // another button has its own history
    }

    #[test]
    fn v120_lines_carry_remainders() {
        let (line, remainder) = lines(90, 0);
        assert_eq!((line, remainder), (0, 90));
        assert_eq!(lines(90, remainder), (1, 60));
        assert_eq!(lines(-90, 0), (0, -90));
        assert_eq!(lines(-90, -90), (-1, -60));
        assert_eq!(lines(-60, 60), (0, 0));
        assert_eq!(lines(i32::MAX, 119), (17_895_698, 6));
        assert_eq!(lines(i32::MIN, -119), (-17_895_698, -7));
        // The native pixel constructor also needs carry, but in points rather than v120 units.
        assert_eq!(pixel_delta(0.75, 0.0).ok(), Some((0, 0.75)));
        assert_eq!(pixel_delta(0.75, 0.75).ok(), Some((1, 0.5)));
        assert_eq!(pixel_delta(-0.75, -0.75).ok(), Some((-1, -0.5)));
        assert!(pixel_delta(f64::NAN, 0.0).is_err());
        assert_eq!(Gesture::MayBegin.end_phase(), ScrollPhase::Cancelled);
        assert_eq!(Gesture::Touch.end_phase(), ScrollPhase::Ended);
        assert_eq!(Gesture::Momentum.end_phase(), ScrollPhase::MomentumEnded);
    }

    #[test]
    fn horizontal_scroll_sign_at_quartz_output() {
        assert_eq!(quartz_scroll(1, 2), [2, -1]);
        assert_eq!(quartz_scroll(-1, -2), [-2, 1]);
        assert_eq!(quartz_scroll(i32::MIN, 0), [0, i32::MAX]);
        let (x, remainder) = lines(90, 0);
        assert_eq!((quartz_scroll(x, 0), remainder), ([0, 0], 90));
        let (x, remainder) = lines(90, remainder);
        assert_eq!((quartz_scroll(x, 0), remainder), ([0, -1], 60));
        let (x, remainder) = lines(-180, remainder);
        assert_eq!((quartz_scroll(x, 0), remainder), ([0, 1], 0));
        let (x, remainder) = pixel_delta(0.75, 0.0).expect("pixel delta");
        assert_eq!((quartz_scroll(x, 0), remainder), ([0, 0], 0.75));
        let (x, remainder) = pixel_delta(0.75, remainder).expect("pixel delta");
        assert_eq!((quartz_scroll(x, 0), remainder), ([0, -1], 0.5));
        let (x, remainder) = pixel_delta(-1.5, remainder).expect("pixel delta");
        assert_eq!((quartz_scroll(x, 0), remainder), ([0, 1], 0.0));
        let (x, _) = pixel_delta(f64::from(i32::MIN), 0.0).expect("minimum pixel delta");
        assert_eq!(quartz_scroll(x, 0), [0, i32::MAX]);
    }

    #[test]
    fn display_point_at_scale_two() {
        let bounds = CGRect::new(CGPoint::new(-100.0, 50.0), CGSize::new(400.0, 300.0));
        assert_eq!(
            display_point(bounds, PointDevice::new(200.0, 300.0), 2.0),
            CGPoint::new(0.0, 200.0)
        );
        assert_eq!(
            display_point(bounds, PointDevice::new(-50.0, -1.0), 2.0),
            bounds.origin
        );
        assert_eq!(
            display_point(bounds, PointDevice::new(800.0, 600.0), 2.0),
            CGPoint::new(299.5, 349.5)
        );
        assert_eq!(
            mouse_delta(CGPoint::new(100.5, 8.0), CGPoint::new(98.0, 10.0)),
            [3, -2]
        );
        let posted_point = CGPoint::new(100.0, 200.0);
        let observed = CGPoint::new(0.0, 0.0);
        let at = MonoTime::from_nanos(1_000_000_000);
        let now = at.saturating_add(Duration::from_millis(1));
        let posted = Some((posted_point, at));
        assert_eq!(latest_point(posted, observed, now, 0.002), posted_point);
        assert_eq!(latest_point(posted, observed, now, 0.0005), observed);
        assert_eq!(latest_point(posted, observed, now, f64::NAN), observed);
        assert_eq!(latest_point(None, observed, now, 0.002), observed);
    }

    #[test]
    fn repeat_schedule_with_injected_clock() {
        let delay = Duration::from_millis(500);
        let interval = Duration::from_millis(83);
        for seconds in [0.0, f64::NAN, f64::INFINITY, -1.0] {
            assert_eq!(duration(seconds, delay), delay);
            assert_eq!(duration(seconds, interval), interval);
        }
        assert_eq!(duration(0.25, delay), Duration::from_millis(250));
        let mut schedule = RepeatSchedule {
            delay,
            interval,
            pending: None,
        };
        let at = |ms: u64| MonoTime::from_nanos(ms * 1_000_000);
        let a = HidUsage::keyboard(0x04);
        let b = HidUsage::keyboard(0x05);
        schedule.press(a, at(0));
        assert_eq!(schedule.due(|| at(499)), None);
        assert_eq!(schedule.due(|| at(500)), Some(a));
        assert_eq!(schedule.due(|| at(582)), None);
        schedule.press(HidUsage::keyboard(0xE1), at(582));
        assert_eq!(schedule.due(|| at(583)), Some(a));
        schedule.press(b, at(600));
        schedule.release(a);
        assert_eq!(schedule.due(|| at(1099)), None);
        assert_eq!(schedule.due(|| at(1100)), Some(b));
        assert_eq!(schedule.due(|| at(3000)), Some(b));
        assert_eq!(schedule.due(|| at(3000)), None); // no catch-up burst
        schedule.release(b);
        assert_eq!(schedule.due(|| at(4000)), None); // doesn't resume an older key
        schedule.press(a, at(4000));
        schedule.stop(); // gate, Secure Input, release_all, recovery and drop use this
        assert_eq!(schedule.due(|| at(5000)), None);
        schedule.press(CAPS_LOCK, at(5000));
        assert_eq!(schedule.due(|| at(6000)), None);
    }
}
