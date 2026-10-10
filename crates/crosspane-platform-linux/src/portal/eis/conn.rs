//! One EIS connection as a sender: handshake, seat and device tracking, and the requests that
//! press, move and scroll. Owned and driven by the worker thread only (the event converter is not
//! `Send`).
//!
//! Every request that emits input is followed by an `ei_device.frame`; a device is sent
//! `start_emulating` before its first event after it (re)resumes, and `stop_emulating` when a
//! release-all leaves nothing held on it. A device the compositor pauses or removes has had its
//! logical state reset, so the ledger forgets it.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crosspane_platform::{IoGate, PlatformError};
use crosspane_types::input::LockKeys;
use reis::PendingRequestResult;
use reis::ei;
use reis::ei::button::ButtonState;
use reis::ei::keyboard::KeyState;
use reis::event::{self, DeviceCapability, EiEvent, EiEventConverter};
use reis::handshake::EiHandshaker;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;

use super::keymap::LockMasks;
use super::ledger::{Axes, DevId, Ledger};
use super::map::{self, Motion, ScrollPlan};
use super::regions::{self, RegionRect};

/// The name the compositor shows for this client.
const CLIENT_NAME: &str = "Crosspane";

pub(super) fn backend(message: &str) -> PlatformError {
    PlatformError::Backend(message.into())
}

pub(super) fn no_session() -> PlatformError {
    backend("no portal input session")
}

fn lost() -> PlatformError {
    backend("EIS connection lost")
}

fn timespec(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(duration.subsec_nanos()),
    }
}

/// `CLOCK_MONOTONIC` in microseconds, the timebase of `ei_device.frame`.
fn now_micros() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(now.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000_000)
        .saturating_add(u64::try_from(now.tv_nsec).unwrap_or(0) / 1_000)
}

/// Wait until the socket is readable, `deadline` passes or a signal arrives.
fn wait_readable(context: &ei::Context, deadline: Instant) -> Result<(), PlatformError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(PlatformError::Timeout);
    }
    let mut fds = [PollFd::new(context, PollFlags::IN)];
    match poll(&mut fds, Some(&timespec(remaining))) {
        Ok(_) | Err(Errno::INTR) => Ok(()),
        Err(_) => Err(backend("could not poll the EIS socket")),
    }
}

/// A device that has at least one capability this source uses.
struct Dev {
    id: DevId,
    device: event::Device,
    keyboard: Option<ei::Keyboard>,
    pointer_absolute: Option<ei::PointerAbsolute>,
    button: Option<ei::Button>,
    scroll: Option<ei::Scroll>,
    regions: Vec<RegionRect>,
    lock_masks: Option<LockMasks>,
    resumed: bool,
    emulating: bool,
}

pub(super) struct Conn {
    gate: Arc<IoGate>,
    context: ei::Context,
    converter: EiEventConverter,
    devices: Vec<Dev>,
    next_id: DevId,
    /// The last `start_emulating` sequence number; it must only go up.
    sequence: u32,
    ledger: Ledger,
    locks: LockKeys,
    /// The device absolute motion went to last: the one buttons and scrolling prefer.
    active_pointer: Option<DevId>,
    /// Requests are queued in the socket buffer, not yet written.
    unflushed: bool,
    /// The connection failed or the compositor ended it; the worker drops it.
    dead: bool,
}

impl Conn {
    /// Handshake as a sender on the portal's EIS socket. Bounded by `budget`.
    pub(super) fn connect(
        fd: OwnedFd,
        gate: Arc<IoGate>,
        budget: Duration,
    ) -> Result<Conn, PlatformError> {
        let deadline = Instant::now() + budget;
        let context = ei::Context::new(UnixStream::from(fd))
            .map_err(|_| backend("could not use the EIS socket"))?;
        let mut shaker = EiHandshaker::new(CLIENT_NAME, ei::handshake::ContextType::Sender);
        let response = 'handshake: loop {
            wait_readable(&context, deadline)?;
            context
                .read()
                .map_err(|_| backend("EIS socket closed during the handshake"))?;
            while let Some(result) = context.pending_event() {
                match result {
                    PendingRequestResult::Request(event) => {
                        let step = shaker
                            .handle_event(event)
                            .map_err(|_| backend("EIS handshake failed"))?;
                        if let Some(response) = step {
                            break 'handshake response;
                        }
                    }
                    PendingRequestResult::ParseError(_) => {
                        return Err(backend("EIS protocol error during the handshake"));
                    }
                    PendingRequestResult::InvalidObject(_) => {}
                }
            }
        };
        let converter = EiEventConverter::new(&context, response);
        let mut conn = Conn {
            gate,
            context,
            converter,
            devices: Vec::new(),
            next_id: 1,
            sequence: 0,
            ledger: Ledger::default(),
            locks: LockKeys::default(),
            active_pointer: None,
            unflushed: false,
            dead: false,
        };
        // Whatever followed the handshake in the same read.
        conn.dispatch().map_err(backend)?;
        if conn.dead {
            return Err(lost());
        }
        Ok(conn)
    }

    // ---- state the worker reads -----------------------------------------------------------

    pub(super) fn context(&self) -> &ei::Context {
        &self.context
    }

    pub(super) fn is_dead(&self) -> bool {
        self.dead
    }

    /// A keyboard and an absolute pointer are resumed.
    pub(super) fn is_live(&self) -> bool {
        !self.dead
            && self
                .devices
                .iter()
                .any(|d| d.resumed && d.keyboard.is_some())
            && self
                .devices
                .iter()
                .any(|d| d.resumed && d.pointer_absolute.is_some())
    }

    /// The latest Caps and Num Lock state the compositor reported (all `None` before the first).
    pub(super) fn lock_keys(&self) -> LockKeys {
        self.locks
    }

    pub(super) fn wants_write(&self) -> bool {
        self.unflushed
    }

    /// Something is held or in progress, so the gate must be watched.
    pub(super) fn needs_tick(&self) -> bool {
        !self.ledger.is_empty()
    }

    // ---- reading --------------------------------------------------------------------------

    /// Read what the compositor sent and apply it. Failure marks the connection dead.
    pub(super) fn pump(&mut self) {
        if self.dead {
            return;
        }
        let closed = self.context.read().is_err();
        let result = self.dispatch();
        if let Err(reason) = result {
            tracing::debug!(reason, "EIS connection ended");
            self.dead = true;
        } else if closed {
            tracing::debug!("EIS peer closed the socket");
            self.dead = true;
        }
    }

    fn dispatch(&mut self) -> Result<(), &'static str> {
        while let Some(result) = self.context.pending_event() {
            match result {
                PendingRequestResult::Request(event) => self
                    .converter
                    .handle_event(event)
                    .map_err(|_| "EIS protocol violation")?,
                PendingRequestResult::ParseError(_) => return Err("EIS message did not parse"),
                PendingRequestResult::InvalidObject(_) => {}
            }
        }
        while let Some(event) = self.converter.next_event() {
            self.on_event(event)?;
        }
        Ok(())
    }

    fn on_event(&mut self, event: EiEvent) -> Result<(), &'static str> {
        match event {
            EiEvent::Disconnected(disconnected) => {
                tracing::debug!(reason = ?disconnected.reason, "EIS compositor disconnected us");
                return Err("EIS compositor disconnected us");
            }
            EiEvent::SeatAdded(added) => {
                added.seat.bind_capabilities(
                    DeviceCapability::Keyboard
                        | DeviceCapability::Pointer
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Button
                        | DeviceCapability::Scroll,
                );
                self.flush_now();
            }
            EiEvent::DeviceAdded(added) => self.add_device(added.device),
            EiEvent::DeviceRemoved(removed) => {
                if let Some(index) = self.find(&removed.device) {
                    let dev = self.devices.remove(index);
                    self.forget(dev.id);
                    if !self.devices.iter().any(|d| d.lock_masks.is_some()) {
                        self.locks = LockKeys::default();
                    }
                }
            }
            EiEvent::DeviceResumed(resumed) => {
                if let Some(index) = self.find(&resumed.device)
                    && let Some(dev) = self.devices.get_mut(index)
                {
                    // A device that was paused is already not emulating; one that is resumed
                    // again without a pause in between still is.
                    dev.resumed = true;
                }
            }
            EiEvent::DevicePaused(paused) => {
                if let Some(index) = self.find(&paused.device)
                    && let Some(dev) = self.devices.get_mut(index)
                {
                    // Pausing resets the device: whatever was down is up again.
                    dev.resumed = false;
                    dev.emulating = false;
                    let id = dev.id;
                    self.forget(id);
                }
            }
            EiEvent::KeyboardModifiers(modifiers) => {
                if let Some(index) = self.find(&modifiers.device)
                    && let Some(masks) = self.devices.get(index).and_then(|d| d.lock_masks)
                {
                    self.locks = masks.read(modifiers.locked);
                }
            }
            // Receiver-side events, and seats going away (their devices follow).
            _ => {}
        }
        Ok(())
    }

    fn add_device(&mut self, device: event::Device) {
        let keyboard = device.interface::<ei::Keyboard>();
        let pointer_absolute = device.interface::<ei::PointerAbsolute>();
        let button = device.interface::<ei::Button>();
        let scroll = device.interface::<ei::Scroll>();
        let wanted = keyboard.is_some()
            || pointer_absolute.is_some()
            || button.is_some()
            || scroll.is_some();
        // A v3 device is paused until the client says its configuration is complete.
        if device.device().version() >= 3 {
            device.device().ready();
            self.flush_now();
        }
        if !wanted {
            return;
        }
        let regions = if pointer_absolute.is_some() {
            device
                .regions()
                .iter()
                .map(|r| RegionRect::new(r.x, r.y, r.width, r.height))
                .collect()
        } else {
            Vec::new()
        };
        let lock_masks = if keyboard.is_some() {
            device.keymap().and_then(LockMasks::from_keymap)
        } else {
            None
        };
        let id = self.next_id;
        self.next_id += 1;
        tracing::debug!(
            keyboard = keyboard.is_some(),
            absolute = pointer_absolute.is_some(),
            button = button.is_some(),
            scroll = scroll.is_some(),
            regions = regions.len(),
            "EIS device added"
        );
        self.devices.push(Dev {
            id,
            device,
            keyboard,
            pointer_absolute,
            button,
            scroll,
            regions,
            lock_masks,
            resumed: false,
            emulating: false,
        });
    }

    fn find(&self, device: &event::Device) -> Option<usize> {
        self.devices.iter().position(|d| d.device == *device)
    }

    fn index_of(&self, id: DevId) -> Option<usize> {
        self.devices.iter().position(|d| d.id == id)
    }

    fn forget(&mut self, id: DevId) {
        self.ledger.forget_device(id);
        if self.active_pointer == Some(id) {
            self.active_pointer = None;
        }
    }

    // ---- flushing -------------------------------------------------------------------------

    /// One write attempt. A full socket buffer leaves the requests queued; the worker retries on
    /// writability. Only a real failure ends the connection.
    fn flush_now(&mut self) {
        match self.context.flush() {
            Ok(()) => self.unflushed = false,
            Err(Errno::AGAIN) => self.unflushed = true,
            Err(_) => self.dead = true,
        }
    }

    /// The worker's retry of an earlier partial flush.
    pub(super) fn retry_flush(&mut self) {
        if self.unflushed && !self.dead {
            self.flush_now();
        }
    }

    /// Write everything queued, waiting for the socket until `deadline`. On timeout the requests
    /// stay queued (and are still written later), so callers treat their effects as submitted.
    fn flush_until(&mut self, deadline: Instant) -> Result<(), PlatformError> {
        loop {
            match self.context.flush() {
                Ok(()) => {
                    self.unflushed = false;
                    return Ok(());
                }
                Err(Errno::AGAIN) => {
                    self.unflushed = true;
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(PlatformError::Timeout);
                    }
                    let mut fds = [PollFd::new(&self.context, PollFlags::OUT)];
                    match poll(&mut fds, Some(&timespec(remaining))) {
                        Ok(_) | Err(Errno::INTR) => {}
                        Err(_) => {
                            self.dead = true;
                            return Err(lost());
                        }
                    }
                }
                Err(_) => {
                    self.dead = true;
                    return Err(lost());
                }
            }
        }
    }

    /// The outcome of the flush that ends a release. A dead connection counts as released: the
    /// compositor drops a vanished client's devices and with them what was held.
    fn flush_release(&mut self, deadline: Instant) -> Result<(), PlatformError> {
        match self.flush_until(deadline) {
            Err(_) if self.dead => Ok(()),
            other => other,
        }
    }

    // ---- requests -------------------------------------------------------------------------

    fn serial(&self) -> u32 {
        self.converter.connection().serial()
    }

    fn frame(&self, index: usize) {
        if let Some(dev) = self.devices.get(index) {
            dev.device.device().frame(self.serial(), now_micros());
        }
    }

    /// `start_emulating` once per resume, before the device's first event.
    fn start(&mut self, index: usize) {
        let serial = self.serial();
        if let Some(dev) = self.devices.get_mut(index)
            && !dev.emulating
        {
            self.sequence = self.sequence.wrapping_add(1);
            dev.device.device().start_emulating(serial, self.sequence);
            dev.emulating = true;
        }
    }

    /// `stop_emulating` on every device that has nothing held and is emulating.
    fn settle(&mut self) {
        let serial = self.serial();
        for dev in &mut self.devices {
            if dev.emulating && self.ledger.is_idle(dev.id) {
                dev.device.device().stop_emulating(serial);
                dev.emulating = false;
            }
        }
    }

    /// Checked immediately before anything that presses, moves or scrolls.
    fn admit(&self, deadline: Instant) -> Result<(), PlatformError> {
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if !self.is_live() {
            return Err(no_session());
        }
        Ok(())
    }

    fn keyboard(&self) -> Option<(usize, ei::Keyboard)> {
        self.devices.iter().enumerate().find_map(|(i, d)| {
            d.resumed
                .then(|| d.keyboard.clone())
                .flatten()
                .map(|k| (i, k))
        })
    }

    /// The device pointer buttons and scrolling go through: the one absolute motion used last
    /// when it can, else the first that can.
    fn pick<T>(&self, handle: impl Fn(&Dev) -> Option<T>) -> Option<(usize, T)> {
        let preferred = self
            .active_pointer
            .and_then(|id| self.index_of(id))
            .and_then(|i| {
                self.devices
                    .get(i)
                    .filter(|d| d.resumed)
                    .and_then(&handle)
                    .map(|h| (i, h))
            });
        preferred.or_else(|| {
            self.devices
                .iter()
                .enumerate()
                .find_map(|(i, d)| d.resumed.then(|| handle(d)).flatten().map(|h| (i, h)))
        })
    }

    // ---- keys -----------------------------------------------------------------------------

    pub(super) fn press_key(&mut self, code: u16, deadline: Instant) -> Result<(), PlatformError> {
        self.admit(deadline)?;
        if self.ledger.key_device(code).is_some() {
            // Already down; the caller sends transitions only, and a repeat is the compositor's.
            return Ok(());
        }
        let (index, keyboard) = self.keyboard().ok_or_else(no_session)?;
        let id = self.devices.get(index).map_or(0, |d| d.id);
        self.start(index);
        // Held from here: the down may reach the compositor even if the flush below times out.
        self.ledger.hold_key(code, id);
        keyboard.key(u32::from(code), KeyState::Press);
        self.frame(index);
        self.flush_until(deadline)
    }

    /// Queue the release of one held key. False when it wasn't held.
    fn queue_key_up(&mut self, code: u16) -> bool {
        let Some(id) = self.ledger.drop_key(code) else {
            return false;
        };
        if let Some(index) = self.index_of(id)
            && let Some(keyboard) = self.devices.get(index).and_then(|d| d.keyboard.clone())
        {
            keyboard.key(u32::from(code), KeyState::Released);
            self.frame(index);
        }
        true
    }

    pub(super) fn release_key(
        &mut self,
        code: u16,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        self.queue_key_up(code);
        self.flush_release(deadline)
    }

    /// Release every key held, then let go of devices with nothing left.
    pub(super) fn release_keys(&mut self, deadline: Instant) -> Result<(), PlatformError> {
        for (id, codes) in self.ledger.keys_by_device() {
            for &code in &codes {
                self.ledger.drop_key(code);
            }
            if let Some(index) = self.index_of(id)
                && let Some(keyboard) = self.devices.get(index).and_then(|d| d.keyboard.clone())
            {
                for code in codes {
                    keyboard.key(u32::from(code), KeyState::Released);
                }
                self.frame(index);
            }
        }
        self.settle();
        self.flush_release(deadline)
    }

    /// Crash recovery: release those of `codes` this connection holds.
    pub(super) fn recover_keys(
        &mut self,
        codes: &[u16],
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        for &code in codes {
            self.queue_key_up(code);
        }
        self.settle();
        self.flush_release(deadline)
    }

    /// Tap Caps/Num Lock where the compositor's state differs from `wanted`.
    pub(super) fn set_locks(
        &mut self,
        wanted: LockKeys,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        self.admit(deadline)?;
        let (index, keyboard) = self.keyboard().ok_or_else(no_session)?;
        let asked = wanted.caps_lock.is_some() || wanted.num_lock.is_some();
        let known = self
            .devices
            .get(index)
            .is_some_and(|d| d.lock_masks.is_some());
        if asked && !known {
            // Without the keymap the compositor's modifier reports can't be read, so a tap
            // can't be judged necessary.
            return Err(PlatformError::Unsupported(
                "EIS keyboard has no keymap to read lock keys from",
            ));
        }
        for (code, wanted, current) in [
            (map::KEY_CAPSLOCK, wanted.caps_lock, self.locks.caps_lock),
            (map::KEY_NUMLOCK, wanted.num_lock, self.locks.num_lock),
        ] {
            let Some(wanted) = wanted else {
                continue;
            };
            // The compositor sends modifiers after a resume when any is set, so none yet means
            // the lock is off.
            if current.unwrap_or(false) == wanted || self.ledger.key_device(code).is_some() {
                continue;
            }
            self.start(index);
            keyboard.key(u32::from(code), KeyState::Press);
            self.frame(index);
            keyboard.key(u32::from(code), KeyState::Released);
            self.frame(index);
            // Until the compositor's own report arrives.
            if code == map::KEY_CAPSLOCK {
                self.locks.caps_lock = Some(wanted);
            } else {
                self.locks.num_lock = Some(wanted);
            }
        }
        self.flush_until(deadline)
    }

    // ---- pointer --------------------------------------------------------------------------

    /// Absolute motion to a point in the desktop's logical space.
    pub(super) fn move_to(
        &mut self,
        x: f64,
        y: f64,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        self.admit(deadline)?;
        let candidates: Vec<(DevId, RegionRect)> = self
            .devices
            .iter()
            .filter(|d| d.resumed && d.pointer_absolute.is_some())
            .flat_map(|d| d.regions.iter().map(|r| (d.id, *r)))
            .collect();
        let target = regions::locate(&candidates, self.active_pointer, x, y)
            .ok_or(PlatformError::NotFound)?;
        let index = self.index_of(target.key).ok_or(PlatformError::NotFound)?;
        let pointer = self
            .devices
            .get(index)
            .and_then(|d| d.pointer_absolute.clone())
            .ok_or(PlatformError::NotFound)?;
        self.start(index);
        pointer.motion_absolute(target.x, target.y);
        self.frame(index);
        self.active_pointer = Some(target.key);
        self.flush_until(deadline)
    }

    pub(super) fn press_button(
        &mut self,
        code: u32,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        self.admit(deadline)?;
        if self.ledger.button_device(code).is_some() {
            return Ok(());
        }
        let (index, button) = self
            .pick(|d| d.button.clone())
            .ok_or(PlatformError::Unsupported(
                "no resumed EIS device has the button capability",
            ))?;
        let id = self.devices.get(index).map_or(0, |d| d.id);
        self.start(index);
        self.ledger.hold_button(code, id);
        button.button(code, ButtonState::Press);
        self.frame(index);
        self.flush_until(deadline)
    }

    fn queue_button_up(&mut self, code: u32) -> bool {
        let Some(id) = self.ledger.drop_button(code) else {
            return false;
        };
        if let Some(index) = self.index_of(id)
            && let Some(button) = self.devices.get(index).and_then(|d| d.button.clone())
        {
            button.button(code, ButtonState::Released);
            self.frame(index);
        }
        true
    }

    pub(super) fn release_button(
        &mut self,
        code: u32,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        self.queue_button_up(code);
        self.flush_release(deadline)
    }

    /// Release every button held and end any smooth scroll.
    pub(super) fn release_buttons(&mut self, deadline: Instant) -> Result<(), PlatformError> {
        for (id, codes) in self.ledger.buttons_by_device() {
            for &code in &codes {
                self.ledger.drop_button(code);
            }
            if let Some(index) = self.index_of(id)
                && let Some(button) = self.devices.get(index).and_then(|d| d.button.clone())
            {
                for code in codes {
                    button.button(code, ButtonState::Released);
                }
                self.frame(index);
            }
        }
        self.stop_scrolls();
        self.settle();
        self.flush_release(deadline)
    }

    pub(super) fn recover_buttons(
        &mut self,
        codes: &[u32],
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        for &code in codes {
            self.queue_button_up(code);
        }
        self.settle();
        self.flush_release(deadline)
    }

    /// Queue a scroll stop on `id` for `axes` and forget the gesture there (queued, not flushed).
    fn queue_scroll_stop(&mut self, id: DevId, axes: Axes, cancel: bool) {
        self.ledger.end_smooth(id, axes);
        if let Some(index) = self.index_of(id)
            && let Some(scroll) = self.devices.get(index).and_then(|d| d.scroll.clone())
        {
            self.start(index);
            scroll.scroll_stop(u32::from(axes.x), u32::from(axes.y), u32::from(cancel));
            self.frame(index);
        }
    }

    /// End every smooth-scroll gesture still in progress (queued, not flushed).
    fn stop_scrolls(&mut self) {
        for (id, axes) in self.ledger.smooth_devices() {
            self.queue_scroll_stop(id, axes, false);
        }
    }

    /// Submit a scroll plan: the displacement in one frame, the stop in the next. A plan with no
    /// displacement only ends a gesture, which a closed gate allows.
    pub(super) fn scroll(
        &mut self,
        plan: ScrollPlan,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        if plan.motion.is_some() {
            self.admit(deadline)?;
        } else if plan.stop.is_none() {
            return Ok(());
        }
        let (index, scroll) = self
            .pick(|d| d.scroll.clone())
            .ok_or(PlatformError::Unsupported(
                "no resumed EIS device has the scroll capability",
            ))?;
        let id = self.devices.get(index).map_or(0, |d| d.id);
        if let Some(motion) = plan.motion {
            self.start(index);
            match motion {
                Motion::Smooth { x, y } => {
                    scroll.scroll(x, y);
                    self.ledger.start_smooth(
                        id,
                        Axes {
                            x: x != 0.0,
                            y: y != 0.0,
                        },
                    );
                }
                Motion::Discrete { x, y } => scroll.scroll_discrete(x, y),
            }
            self.frame(index);
        }
        if let Some(stop) = plan.stop {
            // The end goes to the device that holds the gesture (the pointer may have moved to
            // another device since it began); with none holding it, to the device just used.
            let holding: Vec<(DevId, Axes)> = self
                .ledger
                .smooth_devices()
                .into_iter()
                .filter_map(|(dev, held)| {
                    let axes = Axes {
                        x: held.x && stop.axes.x,
                        y: held.y && stop.axes.y,
                    };
                    axes.any().then_some((dev, axes))
                })
                .collect();
            let mut targets: BTreeMap<DevId, Axes> = BTreeMap::new();
            match holding.as_slice() {
                [] => {
                    targets.insert(id, stop.axes);
                }
                [(only, _)] => {
                    targets.insert(*only, stop.axes);
                }
                several => {
                    // Gestures on more than one device: each ends where it is, and an axis none
                    // of them holds goes with the device just used.
                    let mut rest = stop.axes;
                    for &(dev, axes) in several {
                        targets.insert(dev, axes);
                        rest = Axes {
                            x: rest.x && !axes.x,
                            y: rest.y && !axes.y,
                        };
                    }
                    if rest.any() {
                        let entry = targets.entry(id).or_default();
                        *entry = Axes {
                            x: entry.x || rest.x,
                            y: entry.y || rest.y,
                        };
                    }
                }
            }
            for (target, axes) in targets {
                self.queue_scroll_stop(target, axes, stop.cancel);
            }
        }
        if plan.motion.is_some() {
            self.flush_until(deadline)
        } else {
            self.flush_release(deadline)
        }
    }

    // ---- the gate and the end -------------------------------------------------------------

    /// With the gate closed, let go of everything held and end any smooth scroll: the compositor
    /// repeats a held key, which must not run on into a lock screen.
    pub(super) fn tick(&mut self) {
        if !self.dead && !self.ledger.is_empty() && !self.gate.is_open() {
            let deadline = Instant::now() + Duration::from_millis(20);
            let _ = self.release_keys(deadline);
            let _ = self.release_buttons(deadline);
        }
    }

    /// Release what is held, tell the compositor we are leaving, and write it out, within
    /// `budget`. Best effort; the compositor also drops a closed client's devices.
    pub(super) fn shutdown(&mut self, budget: Duration) {
        if self.dead {
            return;
        }
        let deadline = Instant::now() + budget;
        let _ = self.release_keys(deadline);
        let _ = self.release_buttons(deadline);
        self.converter.connection().connection().disconnect();
        let _ = self.flush_until(deadline);
        self.ledger.clear();
        self.dead = true;
    }
}
