//! Capturing local input on the controller (03 §3 "Pointer capture on the controller").

use std::sync::Arc;

use crosspane_types::geom::{PixelSize, PointDevice};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, WindowId};
use crosspane_types::input::{LockKeys, ScrollDelta};
use crosspane_types::time::MonoTime;

use crate::{EventSink, PlatformError};

/// A side of a display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortalId(pub u32);

/// A stretch of a local display edge where the pointer may leave this node (03 §3 "Layout").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CapturePortal {
    pub id: PortalId,
    pub display: DisplayId,
    pub edge: Edge,
    /// Start and end of the stretch along the edge, in device pixels from the display's top
    /// (left and right edges) or left (top and bottom edges) corner. `from < to`.
    pub from: f64,
    pub to: f64,
}

/// Identifies one capture, chosen by the engine; every capture uses a new ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CaptureId(pub u64);

/// What the engine needs to know at the moment capture becomes effective.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureStart {
    /// Keys held at that moment. Used only to recognise the release chord; never replayed remotely.
    pub held_keys: Vec<HidUsage>,
    pub lock_keys: LockKeys,
}

/// Why a capture ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndReason {
    /// The engine called `end`.
    Requested,
    /// The OS, the compositor or the I/O gate took the capture away, or a required piece (pointer
    /// lock, shortcut inhibition, keyboard visibility) stopped working.
    Lost,
    /// [`CaptureAbort::abort`] ended it.
    Aborted,
}

/// How pointer motion was measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MotionKind {
    /// Not accelerated by the OS, in the input device's own units (Wayland relative-pointer
    /// "unaccelerated" deltas, macOS raw HID deltas). Not guaranteed to be hardware counts; the
    /// engine calibrates them and applies Crosspane's acceleration curve.
    Unaccelerated,
    /// Already accelerated by the OS and converted to device pixels of `display`. The engine must
    /// not accelerate again.
    Accelerated { display: DisplayId },
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum CaptureEvent {
    /// While not capturing: the pointer pressed against a portal. `position` runs from 0.0 at the
    /// portal's `from` to 1.0 at its `to`. Repeats while the pointer keeps pushing.
    EdgePressed {
        portal: PortalId,
        position: f64,
        at: MonoTime,
    },
    /// While not capturing: a local window move (DRAG-v0 D-2) is pressing against `portal`.
    /// `position` runs from 0.0 at the portal's `from` to 1.0 at its `to`. `window` is the moved
    /// window: a `WindowSource` id, or the proxy's own window id when a proxy is moved. `grab` is the
    /// pointer's offset inside that window's content, in device pixels of the display it is on.
    /// Repeats while the pointer keeps pushing; `EdgeReleased` ends it.
    DragAtEdge {
        portal: PortalId,
        position: f64,
        window: WindowId,
        grab: PointDevice,
        at: MonoTime,
    },
    /// While not capturing: a local window move that was pressing against `portal` (DragAtEdge)
    /// ended because its button was released there (DRAG-v0 D-10). Same fields as DragAtEdge.
    /// Reported only by backends whose begin_drag can't capture while the button is held.
    DragDroppedAtEdge {
        portal: PortalId,
        position: f64,
        window: WindowId,
        grab: PointDevice,
        at: MonoTime,
    },
    /// While not capturing: a native window move is in progress under the pointer, reported
    /// without any edge test (DRAG v0-b; the seat decides). `window`: a `WindowSource` id, or the
    /// proxy's own window id when a proxy is moved. `grab`: the pointer's offset inside that
    /// window's content, in device pixels of the display it is on; `size`: the content size.
    /// Backends report it for injected input too (the E1 target case) and at most every 20 ms.
    NativeMove {
        window: WindowId,
        grab: PointDevice,
        size: PixelSize,
        at: MonoTime,
    },
    /// The move reported by `NativeMove` ended (release, window change, lost sample).
    NativeMoveEnded { window: WindowId, at: MonoTime },
    /// The pointer stopped pressing against a portal: it moved away, left the stretch, or the portal
    /// was removed. Cancels any push-to-cross delay.
    EdgeReleased { portal: PortalId, at: MonoTime },
    /// Capture `id` is effective. `Motion`, `Key`, `Button` and `Scroll` events between `Started`
    /// and `Ended` with the same ID belong to it; the engine ignores capture events outside a pair.
    Started { id: CaptureId },
    /// Relative pointer motion.
    Motion {
        dx: f64,
        dy: f64,
        kind: MotionKind,
        at: MonoTime,
    },
    /// A key changed state. OS auto-repeat is never reported.
    Key {
        usage: HidUsage,
        down: bool,
        at: MonoTime,
    },
    /// A pointer button changed state.
    Button {
        button: MouseButton,
        down: bool,
        at: MonoTime,
    },
    /// A scroll step.
    Scroll { delta: ScrollDelta, at: MonoTime },
    /// Capture `id` is over. No event of that capture follows.
    Ended { id: CaptureId, reason: EndReason },
    /// The local lock-key state: delivered after `subscribe` and whenever it changes, ordered
    /// after the `Key` event that caused the change.
    LockKeys(LockKeys),
    /// Whether keyboard events can be observed: `true` while blinded by macOS Secure Event Input.
    /// Delivered after `subscribe` and on every change.
    KeyboardBlinded(bool),
    /// While local-activity monitoring is on and not capturing: physical (not injected) input
    /// happened on this node (local override, best effort; R15).
    LocalActivity { at: MonoTime },
}

/// Ends a capture from any thread without waiting for the thread that owns the capture.
pub trait CaptureAbort: Send + Sync {
    /// Idempotent. Cancels a pending activation, gives input and cursor back to this node, and
    /// emits `Ended { reason: Aborted }` if a capture was active. Takes no locks the capture thread
    /// might hold and never waits for that thread, so the engine's watchdog (04 §6) can use it
    /// when the capture thread is stuck.
    fn abort(&self);
}

/// Captures and suppresses local input while it is routed to another node.
///
/// **Ending a capture** (`end`, loss or abort): the engine releases every key and button it routed
/// to the remote node. For keys and buttons whose downs were suppressed locally, the backend keeps
/// suppressing their repeats and their eventual ups, and doesn't treat them as new downs if a new
/// capture starts while they're still held. On platforms that deliver such a stray release
/// harmlessly (Wayland: the newly focused client sees the key in `wl_keyboard.enter`, not as a
/// press), the backend needn't suppress it (amendment 1, P6a). Anything pressed after the end goes to the local OS
/// at once. Dropping the backend ends any capture within the 50 ms budget.
pub trait InputCapture: Send {
    /// Replace the set of portals atomically: on failure the previous set stays in force.
    fn set_portals(&mut self, portals: &[CapturePortal]) -> Result<(), PlatformError>;

    /// Start delivering events to `sink`. Called once.
    fn subscribe(&mut self, sink: Arc<dyn EventSink<CaptureEvent>>) -> Result<(), PlatformError>;

    /// Begin capture `id`, entered through `portal`: suppress local input, hide the local cursor,
    /// and deliver capture events.
    ///
    /// - Returns `Ok` only once capture is fully effective: pointer capture, keyboard focus and
    ///   shortcut inhibition (where the platform needs them), suppression and cursor hiding. It
    ///   emits `Started { id }` before any capture event. Any failure or timeout rolls back
    ///   everything partial, and nothing requested can activate later.
    /// - Fails with [`PlatformError::Locked`] if the I/O gate is closed,
    ///   [`PlatformError::PointerButtonHeld`] if a pointer button is known to be held, and
    ///   [`PlatformError::SecureInput`] if the keyboard is blinded. Where the platform's implicit
    ///   grab keeps the edge from being entered during a drag (Wayland), "no button event since
    ///   the edge was entered" counts as not held (amendment 1, P6a).
    /// - The caller shows the capture indicator and waits for it to be visible *before* calling this
    ///   (04 §8 invariant 5).
    /// - A key already down at activation is reported in [`CaptureStart::held_keys`], not as a
    ///   `Key` event, and its release reaches the local OS, which saw the press.
    /// - While capturing, the backend ends the capture with `Lost` if the I/O gate closes, the
    ///   keyboard becomes blinded, or any required piece stops working.
    fn begin(&mut self, id: CaptureId, portal: PortalId) -> Result<CaptureStart, PlatformError>;

    /// Like [`InputCapture::begin`], for a window drag across `portal` while `button` is held
    /// (DRAG-v0 D-4). First ends the local native window move and settles the local OS's view of
    /// `button` (macOS: a synthetic up; Hyprland: per P8a), then captures as `begin` does. `button`
    /// must be the only pointer button held, else [`PlatformError::PointerButtonHeld`]. While the
    /// capture runs, the physical release of `button` is delivered exactly once as
    /// `Button { button, down: false }`. If the capture ends first, nothing follows `Ended`, and the
    /// backend keeps suppressing that button's physical tail where the OS would otherwise act on it,
    /// as for suppressed downs. Once the local settlement has happened, that tail suppression also
    /// survives a failed activation and an abort before `Started`: it is excluded from `begin`'s
    /// rollback, so the seat's one down never gets a second up. Every other rule of `begin` applies.
    fn begin_drag(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        button: MouseButton,
    ) -> Result<CaptureStart, PlatformError> {
        let _ = (id, portal, button);
        Err(PlatformError::Unsupported("begin_drag"))
    }

    /// End the current capture and give input back to this node, placing the local pointer at
    /// `warp_to` first if given. Emits `Ended { reason: Requested }`. Must succeed, or fail without
    /// leaving input suppressed.
    fn end(&mut self, warp_to: Option<(DisplayId, PointDevice)>) -> Result<(), PlatformError>;

    /// A handle that can end the capture from another thread.
    fn abort_handle(&self) -> Arc<dyn CaptureAbort>;

    /// Turn local-activity monitoring on or off. Best effort: may return
    /// [`PlatformError::Unsupported`].
    fn set_monitor_local_activity(&mut self, on: bool) -> Result<(), PlatformError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Capture;
    struct Abort;
    impl CaptureAbort for Abort {
        fn abort(&self) {}
    }
    impl InputCapture for Capture {
        fn set_portals(&mut self, _: &[CapturePortal]) -> Result<(), PlatformError> {
            Ok(())
        }
        fn subscribe(&mut self, _: Arc<dyn EventSink<CaptureEvent>>) -> Result<(), PlatformError> {
            Ok(())
        }
        fn begin(&mut self, _: CaptureId, _: PortalId) -> Result<CaptureStart, PlatformError> {
            panic!("begin_drag must not call begin")
        }
        fn end(&mut self, _: Option<(DisplayId, PointDevice)>) -> Result<(), PlatformError> {
            Ok(())
        }
        fn abort_handle(&self) -> Arc<dyn CaptureAbort> {
            Arc::new(Abort)
        }
        fn set_monitor_local_activity(&mut self, _: bool) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    #[test]
    fn begin_drag_defaults_to_unsupported() {
        assert!(matches!(
            Capture.begin_drag(CaptureId(1), PortalId(2), MouseButton::PRIMARY),
            Err(PlatformError::Unsupported("begin_drag"))
        ));
    }
}
