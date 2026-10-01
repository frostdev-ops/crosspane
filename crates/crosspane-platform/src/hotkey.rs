//! The release hotkey outside capture (04 §6).

use std::sync::Arc;

use crosspane_types::hid::HidUsage;
use crosspane_types::time::MonoTime;

use crate::{EventSink, PlatformError};

/// A key with modifiers held, e.g. Ctrl+Alt+Shift+Esc.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub modifiers: Vec<HidUsage>,
    pub key: HidUsage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed { at: MonoTime },
    Released { at: MonoTime },
}

/// Reports the release chord on this node's physical keyboard: on a target (panic: holding it 1 s
/// ends everything) and on a controller (releasing control, panic, re-arming crossing).
///
/// - It is the authority for the chord's press and release timing in every state, including across
///   the start and end of a capture: exactly one `Pressed`/`Released` pair per physical press,
///   with no reset or duplicate at capture boundaries. After `subscribe` it reports the current
///   state first (a `Pressed` if the chord is already held).
/// - During a capture the engine also recognises the chord in the captured `Key` events, so the
///   chord's key is never forwarded to the target.
/// - Injected input never triggers it. The engine owns all timing (the 1 s hold); re-arming needs a
///   release followed by a fresh press.
pub trait GlobalHotkeys: Send {
    /// Watch for `chord`, replacing any previous one. A backend that can't watch for it returns
    /// [`PlatformError::Unsupported`]; it never silently weakens the emergency control.
    fn set_chord(&mut self, chord: &Chord) -> Result<(), PlatformError>;

    /// Start delivering events. Called once.
    fn subscribe(&mut self, sink: Arc<dyn EventSink<HotkeyEvent>>) -> Result<(), PlatformError>;
}
