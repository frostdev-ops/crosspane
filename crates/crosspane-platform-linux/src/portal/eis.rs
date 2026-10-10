//! Key and pointer injection over an EIS socket from the RemoteDesktop portal (WP-G1.4).
//!
//! One worker thread owns the `reis` connection (sender role) and both devices; the two command
//! handles share it, so held modifiers apply to pointer events (09 §1 shared injection source).
//! Calls marshal to the worker and wait at most 45 ms (the frozen 50 ms input-path limit).
//!
//! **Connections.** [`EisSource::attach`] gives the worker an EIS socket (a new portal epoch); it
//! replaces any previous connection after releasing what that one held. [`EisSource::detach`]
//! (the portal session closed) drops it. Without a connection whose seat has a keyboard and a
//! pointer resumed, the source is not live: presses, motion and scrolling fail with
//! `PlatformError::Backend("no portal input session")`; releases succeed (the compositor releases
//! a removed device's keys and buttons, so nothing stays held).
//!
//! **Contract** (`crosspane_platform::inject`, quoted in the task): the [`IoGate`] is checked
//! immediately before every press, move or scroll; releases go through while it is closed. Ups
//! are owed until submitted; `release_all` and drop release everything held. Every press or
//! release is followed by an EIS `frame`. Keys use `hid_to_evdev`; the desktop supplies repeat.
//! Never log key contents.
//!
//! **Regions.** `move_to(display, position)` converts the device-pixel position to the desktop's
//! logical space with the display's `DisplayGeometry` (`logical_origin + position / scale`) from
//! the `displays` snapshot function, and sends absolute motion on the pointer device whose region
//! contains that point. No region containing it is `PlatformError::NotFound`.
//!
//! **Lock keys.** `lock_keys` reads Caps and Num lock from the keyboard's latest `modifiers` event
//! against its keymap (xkbcommon); `None` before the first one. `set_lock_keys` taps the lock key
//! (down, frame, up, frame) when the wanted value differs and that key isn't held by us.
//!
//! **Recovery.** A crashed process's EIS connection died with it and the compositor released its
//! devices, so `recover_keys`/`recover_buttons` release only what this source holds and return Ok.
//!
//! # How it works
//!
//! - **Handshake.** The context type is *sender*, the name `Crosspane`. On each announced seat the
//!   worker binds keyboard, pointer, absolute pointer, button and scroll. A device of `ei_device`
//!   version 3 is sent `ready` when it is announced; the compositor then resumes it. Input goes
//!   only to resumed devices: `start_emulating` (with a sequence number that only goes up) before
//!   a device's first event after it resumes, `frame` after every request group, and
//!   `stop_emulating` when a release-all leaves nothing held on the device. A device the
//!   compositor pauses or removes has had its keys and buttons reset, so the ledger forgets it.
//! - **Ledger.** An entry is made before the down is queued, so a down that may have been sent
//!   counts as held even when the call times out; it is removed when the up is queued. A write
//!   that finds the socket full leaves the requests queued and the worker retries on writability;
//!   a release returns `Timeout` (and is owed again on the next call) until that write is done.
//!   Attaching a new connection, detaching, and dropping the last handle first queue the old
//!   connection's releases, then close it.
//! - **Gate.** Checked in the worker right before each press, move, scroll or lock tap. While
//!   anything is held the worker also looks at the gate every 10 ms, and a closed gate releases all
//!   held keys and buttons and ends any smooth scroll, because the compositor repeats a held key
//!   (there is no repeat to manage here) and it must not run on into a lock screen.
//! - **Scroll.** `pixels` go to `ei_scroll.scroll` for gestures, `v120` to `scroll_discrete` for
//!   wheel detents (never both for one event). Both axes are negated relative to
//!   `ScrollDelta`'s HID convention, like the Hyprland backend. A stop goes in a frame of its own
//!   after the displacement (EIS forbids a displacement and a stop on one axis in a frame); a
//!   `Cancelled` phase is a cancel, every other end a plain stop.
//! - **Buttons and scrolling** use the device absolute motion went to last when it has the
//!   capability, else the first resumed device that has it.
//! - **Absolute motion** targets the region containing the point in the compositor's logical
//!   space; the offset is part of the coordinate. A point within one logical pixel outside every
//!   region (the rounding between a fractional-scale display and its integer region) is moved onto
//!   the nearest region's edge, but only a region that overlaps the logical rectangle of the
//!   display asked for, never a neighbouring display's; anything further is `NotFound`.
//! - **Lock keys.** The compositor reports the locked-modifier mask; Caps and Num Lock are the
//!   keymap's `Lock` and `Mod2` bits. A compositor sends modifiers after a resume only when some
//!   are set, so `set_lock_keys` treats "no report yet" as off (the tap is applied, and the state
//!   updated until the compositor's own report arrives), while `lock_keys` still says `None`.

mod conn;
mod keymap;
mod ledger;
mod map;
mod regions;
mod worker;

#[cfg(test)]
mod tests;

use std::fmt;
use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crosspane_platform::{IoGate, KeyInjector, PlatformError, PointerInjector};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::PointDevice;
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollDelta};
use rustix::event::EventfdFlags;

use self::conn::backend;
use self::worker::{Action, Command, Inner};

/// The frozen 50 ms input-path limit, less scheduling margin.
const CALL_BUDGET: Duration = Duration::from_millis(45);
/// `attach` waits for the handshake (2 s) plus the worker's turn.
const ATTACH_BUDGET: Duration = Duration::from_millis(2_500);
/// `detach` waits for the old connection to write out its releases.
const DETACH_BUDGET: Duration = Duration::from_millis(300);
/// Commands that can wait for the worker; more than this means it is wedged.
const COMMAND_QUEUE: usize = 64;

/// The displays snapshot used for region mapping (the platform's `Displays::displays`).
pub type DisplaysFn = Arc<dyn Fn() -> Vec<DisplayInfo> + Send + Sync>;

/// A hook called with the target display before every absolute pointer move
/// ([`EisSource::set_before_move`]).
pub type MoveHook = Arc<dyn Fn(DisplayId) + Send + Sync>;

/// What the three handles share. When the last one goes, the worker releases what is held, says
/// goodbye to the compositor and stops.
struct Shared {
    commands: SyncSender<Command>,
    inner: Arc<Inner>,
    displays: DisplaysFn,
    /// See [`EisSource::set_before_move`].
    before_move: Mutex<Option<MoveHook>>,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.inner.closing.store(true, Ordering::Release);
        self.inner.wake();
    }
}

impl Shared {
    /// Queue `action` for the worker and wait for its answer until `deadline`. A command the
    /// worker runs after the deadline is refused there if it presses, moves or scrolls; a down
    /// that was already submitted when the wait ran out stays in the worker's ledger.
    fn call(&self, action: Action, deadline: Instant) -> Result<(), PlatformError> {
        let (reply, answer) = mpsc::sync_channel(1);
        let command = Command {
            action,
            deadline,
            reply,
        };
        match self.commands.try_send(command) {
            Ok(()) => {
                self.inner.wake();
                answer
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|error| match error {
                        RecvTimeoutError::Timeout => PlatformError::Timeout,
                        RecvTimeoutError::Disconnected => backend("EIS worker stopped"),
                    })?
            }
            Err(TrySendError::Full(_)) => Err(PlatformError::Timeout),
            Err(TrySendError::Disconnected(_)) => Err(backend("EIS worker stopped")),
        }
    }

    fn call_within(&self, action: Action) -> Result<(), PlatformError> {
        self.call(action, Instant::now() + CALL_BUDGET)
    }

    /// Runs the move hook, if one is set. The hook is cloned out of its lock first, so it may call
    /// back into [`EisSource::set_before_move`].
    fn run_before_move(&self, display: DisplayId) {
        let hook = self
            .before_move
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(display);
        }
    }
}

/// The shared injection source. Cloning gives another handle to the same worker.
#[derive(Clone, Debug)]
pub struct EisSource {
    shared: Arc<Shared>,
}

/// The keyboard half.
#[derive(Debug)]
pub struct EisKeyInjector {
    shared: Arc<Shared>,
}

/// The pointer half.
#[derive(Debug)]
pub struct EisPointerInjector {
    shared: Arc<Shared>,
}

impl EisSource {
    /// Start the worker, not yet connected.
    pub fn new(
        gate: Arc<IoGate>,
        displays: DisplaysFn,
    ) -> Result<(EisSource, EisKeyInjector, EisPointerInjector), PlatformError> {
        let wake = rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
            .map_err(|_| backend("could not create the EIS worker wake-up"))?;
        let inner = Arc::new(Inner::new(wake));
        let (commands, queue) = mpsc::sync_channel(COMMAND_QUEUE);
        worker::spawn(gate, inner.clone(), queue)
            .map_err(|_| backend("could not start the EIS worker"))?;
        let shared = Arc::new(Shared {
            commands,
            inner,
            displays,
            before_move: Mutex::new(None),
        });
        Ok((
            EisSource {
                shared: shared.clone(),
            },
            EisKeyInjector {
                shared: shared.clone(),
            },
            EisPointerInjector { shared },
        ))
    }

    /// Connect to a new EIS socket (handshake as a sender named "Crosspane", bind the seat's
    /// keyboard, pointer, absolute pointer, button and scroll capabilities). Returns once the
    /// handshake is done or failed (bounded: 2 s). Replaces the previous connection.
    pub fn attach(&self, fd: OwnedFd) -> Result<(), PlatformError> {
        self.shared
            .call(Action::Attach(fd), Instant::now() + ATTACH_BUDGET)
    }

    /// Drop the connection (the portal session closed). Idempotent. What the connection held is
    /// released first; this waits up to 300 ms for that to be written.
    pub fn detach(&self) {
        let _ = self
            .shared
            .call(Action::Detach, Instant::now() + DETACH_BUDGET);
    }

    /// Whether a connection with a resumed keyboard and pointer exists now.
    pub fn is_live(&self) -> bool {
        self.shared.inner.live.load(Ordering::Acquire)
    }

    /// Sets (replaces) the hook called with the target display **before every absolute pointer
    /// move** (`PointerInjector::move_to`), on the caller's thread, before the display is looked up
    /// and before the move's 45 ms budget starts. Nothing else (keys, buttons, scrolling) runs it,
    /// and it cannot change or veto the move. It must be quick and must not panic: the GNOME twin
    /// (WP-G2.4) uses it to lower its pointer fence when the move targets the twin's display,
    /// because the fence's barriers stop injected absolute motion as well.
    pub fn set_before_move(&self, hook: MoveHook) {
        *self
            .shared
            .before_move
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }
}

impl KeyInjector for EisKeyInjector {
    fn key(&mut self, usage: HidUsage, down: bool) -> Result<(), PlatformError> {
        let code = map::key_code(usage)?;
        self.shared.call_within(Action::Key { code, down })
    }

    fn lock_keys(&self) -> Result<LockKeys, PlatformError> {
        Ok(worker::unpack_locks(
            self.shared.inner.locks.load(Ordering::Acquire),
        ))
    }

    fn set_lock_keys(&mut self, wanted: LockKeys) -> Result<(), PlatformError> {
        if wanted.caps_lock.is_none() && wanted.num_lock.is_none() {
            return Ok(());
        }
        self.shared.call_within(Action::SetLocks(wanted))
    }

    fn release_all(&mut self) -> Result<(), PlatformError> {
        self.shared.call_within(Action::ReleaseKeys)
    }

    fn recover_keys(&mut self, keys: &[HidUsage]) -> Result<(), PlatformError> {
        // A usage with no evdev code can't have been pressed by this source.
        let codes: Vec<u16> = keys
            .iter()
            .filter_map(|usage| map::key_code(*usage).ok())
            .collect();
        if codes.is_empty() {
            return Ok(());
        }
        self.shared.call_within(Action::RecoverKeys(codes))
    }
}

impl PointerInjector for EisPointerInjector {
    fn move_to(&mut self, display: DisplayId, position: PointDevice) -> Result<(), PlatformError> {
        self.shared.run_before_move(display);
        let deadline = Instant::now() + CALL_BUDGET;
        let displays = (self.shared.displays)();
        let info = displays
            .iter()
            .find(|info| info.id == display)
            .ok_or(PlatformError::NotFound)?;
        let (x, y) = regions::logical_point(&info.geometry, position)?;
        let display = regions::RegionRect::of_display(&info.geometry);
        self.shared.call(Action::Move { x, y, display }, deadline)
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        let code = map::button_code(button)?;
        self.shared.call_within(Action::Button { code, down })
    }

    fn scroll(&mut self, delta: ScrollDelta) -> Result<(), PlatformError> {
        let plan = map::plan_scroll(&delta)?;
        if plan.motion.is_none() && plan.stop.is_none() {
            return Ok(());
        }
        self.shared.call_within(Action::Scroll(plan))
    }

    fn release_all(&mut self) -> Result<(), PlatformError> {
        self.shared.call_within(Action::ReleaseButtons)
    }

    fn recover_buttons(&mut self, buttons: &[MouseButton]) -> Result<(), PlatformError> {
        let codes: Vec<u32> = buttons
            .iter()
            .filter_map(|button| map::button_code(*button).ok())
            .collect();
        if codes.is_empty() {
            return Ok(());
        }
        self.shared.call_within(Action::RecoverButtons(codes))
    }
}

impl Drop for EisKeyInjector {
    fn drop(&mut self) {
        let _ = self.shared.call_within(Action::ReleaseKeys);
    }
}

impl Drop for EisPointerInjector {
    fn drop(&mut self) {
        let _ = self.shared.call_within(Action::ReleaseButtons);
    }
}

/// The before-move hook (WP-G2.4). It does not need a compositor: the hook runs before the move
/// is looked at, so a source with no session shows it.
#[cfg(test)]
mod hook_tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Mutex;

    use crosspane_types::input::ScrollPhase;

    use super::*;

    fn source() -> (EisSource, EisKeyInjector, EisPointerInjector) {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        EisSource::new(gate, Arc::new(Vec::new)).unwrap()
    }

    fn recording() -> (MoveHook, Arc<Mutex<Vec<DisplayId>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (
            Arc::new(move |display| sink.lock().unwrap().push(display)),
            seen,
        )
    }

    #[test]
    fn the_hook_runs_before_every_absolute_move_with_its_display() {
        let (source, _keys, mut pointer) = source();
        let (hook, seen) = recording();
        source.set_before_move(hook);
        // Whatever becomes of the move (no such display, no session), the hook has run first.
        assert!(matches!(
            pointer.move_to(DisplayId(7), PointDevice::new(1.0, 1.0)),
            Err(PlatformError::NotFound)
        ));
        assert!(
            pointer
                .move_to(DisplayId(9), PointDevice::new(-5.0, 1.0))
                .is_err()
        );
        assert_eq!(*seen.lock().unwrap(), [DisplayId(7), DisplayId(9)]);
    }

    #[test]
    fn keys_buttons_and_scrolling_do_not_run_the_hook() {
        let (source, mut keys, mut pointer) = source();
        let (hook, seen) = recording();
        source.set_before_move(hook);
        let _ = keys.key(HidUsage::keyboard(0x04), true);
        let _ = keys.key(HidUsage::keyboard(0x04), false);
        let _ = pointer.button(MouseButton::PRIMARY, true);
        let _ = pointer.button(MouseButton::PRIMARY, false);
        let _ = pointer.scroll(ScrollDelta {
            v120_x: 0,
            v120_y: 120,
            pixels: None,
            phase: ScrollPhase::Discrete,
            stop_x: false,
            stop_y: false,
        });
        let _ = pointer.release_all();
        let _ = keys.release_all();
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn the_hook_can_be_replaced_and_every_handle_shares_it() {
        let (source, _keys, mut pointer) = source();
        // No hook: a move is just a move.
        assert!(
            pointer
                .move_to(DisplayId(1), PointDevice::new(1.0, 1.0))
                .is_err()
        );
        let (first, first_seen) = recording();
        source.set_before_move(first);
        let _ = pointer.move_to(DisplayId(1), PointDevice::new(1.0, 1.0));
        let (second, second_seen) = recording();
        source.clone().set_before_move(second);
        let _ = pointer.move_to(DisplayId(2), PointDevice::new(1.0, 1.0));
        assert_eq!(*first_seen.lock().unwrap(), [DisplayId(1)]);
        assert_eq!(*second_seen.lock().unwrap(), [DisplayId(2)]);
    }

    #[test]
    fn a_hook_may_install_another_hook_without_deadlocking() {
        let (source, _keys, mut pointer) = source();
        let inner = source.clone();
        let (next, next_seen) = recording();
        source.set_before_move(Arc::new(move |_| inner.set_before_move(Arc::clone(&next))));
        let _ = pointer.move_to(DisplayId(1), PointDevice::new(1.0, 1.0));
        let _ = pointer.move_to(DisplayId(2), PointDevice::new(1.0, 1.0));
        assert_eq!(*next_seen.lock().unwrap(), [DisplayId(2)]);
    }
}
