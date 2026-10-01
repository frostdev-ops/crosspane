//! Injecting input on the target (03 §3 "Injection on the target").
//!
//! - **The I/O gate.** Injectors check the [`IoGate`](crate::IoGate) immediately before every OS
//!   submission that presses, moves or scrolls, and return [`PlatformError::Locked`] when it is
//!   closed. Releases (key and button ups, `release_all`, recovery) go through even when it is
//!   closed.
//! - **Secure input.** Before every submission that presses, moves, scrolls, repeats or toggles a
//!   lock key, a backend on an OS with secure keyboard entry (macOS Secure Event Input) checks it is
//!   off. On, or not provably off, refuses with [`PlatformError::SecureInput`] and stops any
//!   repetition (04 §7: never inject into secure input fields). Releases still go through.
//! - **Meaning of `Ok`.** The event was submitted to the OS, not that an application received it.
//! - **Ownership.** The reliability contract (04 §8: leases, heartbeats, the journal) lives in the
//!   engine. Injectors track what they themselves hold down so `release_all` can undo it. A down
//!   that may have been submitted before an error counts as held; a release that failed stays
//!   owed. The engine clears its journal only after a release returns `Ok`.
//! - **Shared state.** A platform's key and pointer injectors share one injection source, so held
//!   modifiers apply to pointer events (Shift-click, Command-drag).

use crosspane_types::geom::PointDevice;
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollDelta};

use crate::PlatformError;

/// Injects physical keys. The target's own layout and input method interpret them.
pub trait KeyInjector: Send {
    /// Press or release a physical key. The caller sends transitions only; the backend gives the
    /// key the target's own auto-repeat, natively or by synthesising it from the target's repeat
    /// settings (macOS doesn't repeat posted events). Repeats create no new ownership; a release,
    /// `release_all`, recovery, a closed gate or dropping the injector stops them first.
    fn key(&mut self, usage: HidUsage, down: bool) -> Result<(), PlatformError>;

    /// The current lock-key state of this node (`None` for a lock key it doesn't have or can't read).
    fn lock_keys(&self) -> Result<LockKeys, PlatformError>;

    /// Toggle the lock keys whose wanted value is `Some` and differs from the current state; `None`
    /// leaves a key unchanged. Doesn't change which keys are held.
    fn set_lock_keys(&mut self, wanted: LockKeys) -> Result<(), PlatformError>;

    /// Release every key this injector holds down, preferring an OS mechanism that clears held
    /// state (e.g. destroying the virtual device) where one exists. Idempotent.
    fn release_all(&mut self) -> Result<(), PlatformError>;

    /// After a crash: release keys a previous process pressed, as listed in the journal (04 §8
    /// invariant 2), regardless of this injector's own ledger. May send releases for keys that
    /// were already up; that is acceptable.
    fn recover_keys(&mut self, keys: &[HidUsage]) -> Result<(), PlatformError>;
}

/// Injects pointer motion, buttons and scrolling.
pub trait PointerInjector: Send {
    /// Move the pointer to an absolute device-pixel position on one of this node's displays.
    fn move_to(&mut self, display: DisplayId, position: PointDevice) -> Result<(), PlatformError>;

    /// Press or release a pointer button.
    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError>;

    /// Scroll at the current pointer position. Ending injection (`release_all`, a closed gate,
    /// dropping the injector) also ends any smooth-scroll gesture still in progress.
    fn scroll(&mut self, delta: ScrollDelta) -> Result<(), PlatformError>;

    /// Release every button this injector holds down. Idempotent.
    fn release_all(&mut self) -> Result<(), PlatformError>;

    /// After a crash: release buttons a previous process pressed, as listed in the journal.
    fn recover_buttons(&mut self, buttons: &[MouseButton]) -> Result<(), PlatformError>;
}
