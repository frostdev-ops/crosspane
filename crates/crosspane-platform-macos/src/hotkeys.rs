//! Release/panic chord observation from the existing capture tap.
//!
//! No additional tap or native registration is created. Secure Event Input can hide keyboard
//! events; this backend never bypasses it. The handle's lifetime depends on its original capture
//! backend: after that backend stops or dies, no further hotkey events are delivered and subsequent
//! calls return Unsupported. There is no asynchronous hotkey loss event; existing capture failure
//! is the signal that the backend has ended.

use std::sync::Arc;

use crosspane_platform::{Chord, EventSink, GlobalHotkeys, HotkeyEvent, PlatformError};
use crosspane_types::{hid::hid_to_macos, time::MonoTime};

use crate::capture::{HotkeyTap, MacCapture};

/// Observes the original capture tap; it never creates a tap, thread or native registration.
/// Construction configures the caller's engine chord before the existing agent subscription.
/// Input Monitoring and Accessibility must both be available to that capture backend. Secure
/// Event Input remains unobservable. After the original tap/backend stops or dies, no further
/// hotkey events are delivered and subsequent methods return Unsupported. The frozen trait has
/// no asynchronous hotkey loss event; existing capture failure is the backend-loss signal.
pub struct MacHotkeys {
    tap: HotkeyTap,
}

impl std::fmt::Debug for MacHotkeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MacHotkeys(..)")
    }
}

impl MacHotkeys {
    pub fn new(capture: &MacCapture, chord: &Chord) -> Result<Self, PlatformError> {
        let mut hotkeys = Self {
            tap: capture.hotkey_tap()?,
        };
        hotkeys.set_chord(chord)?;
        Ok(hotkeys)
    }
}

impl GlobalHotkeys for MacHotkeys {
    fn set_chord(&mut self, chord: &Chord) -> Result<(), PlatformError> {
        self.tap.set(chord)
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<HotkeyEvent>>) -> Result<(), PlatformError> {
        self.tap.subscribe(sink)
    }
}

#[derive(PartialEq)]
struct WatchedChord {
    key: usize,
    modifiers: Vec<(usize, u8)>,
}

pub(crate) struct ChordState {
    chord: Option<WatchedChord>,
    held: [bool; 128],
    modifiers: u8,
    pressed: Option<MonoTime>,
}

impl Default for ChordState {
    fn default() -> Self {
        Self {
            chord: None,
            held: [false; 128],
            modifiers: 0,
            pressed: None,
        }
    }
}

fn watched(chord: &Chord) -> Result<WatchedChord, PlatformError> {
    let invalid = || PlatformError::Unsupported("macOS release chord");
    // Caps Lock's toggle flags do not provide an ordinary physical press/release pair.
    let key = hid_to_macos(chord.key)
        .filter(|code| *code < 128 && *code != 0x39)
        .ok_or_else(invalid)?;
    if chord.modifiers.len() > 8 {
        return Err(invalid());
    }
    let mut modifiers = Vec::with_capacity(chord.modifiers.len());
    for usage in &chord.modifiers {
        let bit = (0..8)
            .find(|bit| *usage == crosspane_types::hid::HidUsage::keyboard(0xe0 + bit))
            .ok_or_else(invalid)?;
        let code = hid_to_macos(*usage)
            .filter(|code| *code < 128)
            .ok_or_else(invalid)?;
        let entry = (usize::from(code), 1 << bit);
        if modifiers.contains(&entry) || code == key {
            return Err(invalid());
        }
        modifiers.push(entry);
    }
    modifiers.sort_unstable();
    Ok(WatchedChord {
        key: usize::from(key),
        modifiers,
    })
}

impl ChordState {
    pub(crate) fn validate(chord: &Chord) -> Result<(), PlatformError> {
        watched(chord).map(|_| ())
    }

    pub(crate) fn release_for_replacement(
        &mut self,
        chord: &Chord,
        at: MonoTime,
    ) -> Result<Option<HotkeyEvent>, PlatformError> {
        let next = watched(chord)?;
        if self.chord.as_ref().is_some_and(|current| *current != next)
            && self.pressed.take().is_some()
        {
            Ok(Some(HotkeyEvent::Released { at }))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn set(
        &mut self,
        chord: &Chord,
        held: [bool; 128],
        modifiers: u8,
        at: MonoTime,
    ) -> Result<Option<HotkeyEvent>, PlatformError> {
        let next = watched(chord)?;
        self.chord = Some(next);
        Ok(self.refresh(held, modifiers, at))
    }

    pub(crate) fn refresh(
        &mut self,
        held: [bool; 128],
        modifiers: u8,
        at: MonoTime,
    ) -> Option<HotkeyEvent> {
        self.held = held;
        self.modifiers = modifiers;
        self.transition(at)
    }

    pub(crate) fn key(
        &mut self,
        code: u16,
        down: bool,
        modifiers: u8,
        physical: bool,
        at: MonoTime,
    ) -> Option<HotkeyEvent> {
        if !physical {
            return None;
        }
        let held = self.held.get_mut(usize::from(code))?;
        *held = down;
        self.modifiers = modifiers;
        self.transition(at)
    }

    fn transition(&mut self, at: MonoTime) -> Option<HotkeyEvent> {
        let down = self.chord.as_ref().is_some_and(|chord| {
            self.held[chord.key]
                && chord
                    .modifiers
                    .iter()
                    .all(|(code, flag)| self.held[*code] && self.modifiers & flag != 0)
        });
        match (self.pressed, down) {
            (None, true) => {
                self.pressed = Some(at);
                Some(HotkeyEvent::Pressed { at })
            }
            (Some(_), false) => {
                self.pressed = None;
                Some(HotkeyEvent::Released { at })
            }
            _ => None,
        }
    }

    pub(crate) fn current(&self, at: MonoTime) -> HotkeyEvent {
        self.pressed
            .map_or(HotkeyEvent::Released { at }, |at| HotkeyEvent::Pressed {
                at,
            })
    }
}

/// This is the existing LocalActivity classifier, not hardware authentication. Actual hardware
/// source-field coverage is unmeasured; nonmatching and unknown sources are ignored.
pub(crate) fn physical_source(tag: i64, pid: i64, state: i64, own_tag: i64) -> bool {
    tag != own_tag && pid == 0 && state == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_types::hid::HidUsage;

    const CONTROL: u16 = 0x3b;
    const SHIFT: u16 = 0x38;
    const ALT: u16 = 0x3a;
    const ESCAPE: u16 = 0x35;
    const ALL_MODIFIERS: u8 = 0b0000_0111;

    fn at(n: u64) -> MonoTime {
        MonoTime::from_nanos(n)
    }

    fn chord() -> Chord {
        Chord {
            modifiers: vec![
                HidUsage::keyboard(0xe0),
                HidUsage::keyboard(0xe1),
                HidUsage::keyboard(0xe2),
            ],
            key: HidUsage::keyboard(0x29),
        }
    }

    fn watching() -> ChordState {
        let mut state = ChordState::default();
        assert_eq!(state.set(&chord(), [false; 128], 0, at(0)).unwrap(), None);
        state
    }

    fn press_modifiers(state: &mut ChordState) {
        for (i, (code, flags)) in [(CONTROL, 1), (SHIFT, 3), (ALT, ALL_MODIFIERS)]
            .into_iter()
            .enumerate()
        {
            assert_eq!(state.key(code, true, flags, true, at(i as u64 + 1)), None);
        }
    }

    #[test]
    fn chord_down_emits_one_pressed_despite_repeat_and_duplicate_down() {
        let mut state = watching();
        press_modifiers(&mut state);
        assert_eq!(
            state.key(ESCAPE, true, ALL_MODIFIERS, true, at(4)),
            Some(HotkeyEvent::Pressed { at: at(4) })
        );
        for tick in 5..10 {
            assert_eq!(state.key(ESCAPE, true, ALL_MODIFIERS, true, at(tick)), None);
        }
    }

    #[test]
    fn partial_chord_and_injected_modifier_flags_emit_nothing() {
        let mut state = watching();
        assert_eq!(state.key(ESCAPE, true, ALL_MODIFIERS, true, at(1)), None);
        assert_eq!(state.key(ESCAPE, false, ALL_MODIFIERS, true, at(2)), None);
        assert_eq!(state.key(CONTROL, true, 1, true, at(3)), None);
        assert_eq!(state.key(SHIFT, true, 3, true, at(4)), None);
        assert_eq!(state.key(ESCAPE, true, 3, true, at(5)), None);
    }

    #[test]
    fn release_of_any_part_emits_one_released_then_fresh_press() {
        for (released, remaining) in [
            (CONTROL, 0b110),
            (SHIFT, 0b101),
            (ALT, 0b011),
            (ESCAPE, ALL_MODIFIERS),
        ] {
            let mut state = watching();
            press_modifiers(&mut state);
            assert_eq!(
                state.key(ESCAPE, true, ALL_MODIFIERS, true, at(4)),
                Some(HotkeyEvent::Pressed { at: at(4) })
            );
            assert_eq!(
                state.key(released, false, remaining, true, at(5)),
                Some(HotkeyEvent::Released { at: at(5) })
            );
            assert_eq!(state.key(released, false, remaining, true, at(6)), None);
            assert_eq!(
                state.key(released, true, ALL_MODIFIERS, true, at(7)),
                Some(HotkeyEvent::Pressed { at: at(7) })
            );
        }
    }

    #[test]
    fn injected_tag_pid_and_non_hid_events_never_press_or_release() {
        const OWN_TAG: i64 = 7;
        for (tag, pid, source) in [(OWN_TAG, 0, 0), (0, 55, 0), (0, 0, 1), (0, 0, -1)] {
            let mut state = watching();
            let physical = physical_source(tag, pid, source, OWN_TAG);
            assert!(!physical);
            for code in [CONTROL, SHIFT, ALT, ESCAPE] {
                assert_eq!(state.key(code, true, ALL_MODIFIERS, physical, at(1)), None);
            }
            press_modifiers(&mut state);
            assert_eq!(
                state.key(ESCAPE, true, ALL_MODIFIERS, true, at(4)),
                Some(HotkeyEvent::Pressed { at: at(4) })
            );
            assert_eq!(state.key(ESCAPE, false, 0, physical, at(5)), None);
            assert_eq!(state.current(at(6)), HotkeyEvent::Pressed { at: at(4) });
            assert_eq!(
                state.key(ESCAPE, false, ALL_MODIFIERS, true, at(7)),
                Some(HotkeyEvent::Released { at: at(7) })
            );
        }
    }

    #[test]
    fn already_held_subscribe_current_state_precedes_later_release() {
        let mut state = ChordState::default();
        let mut held = [false; 128];
        for code in [CONTROL, SHIFT, ALT, ESCAPE] {
            held[usize::from(code)] = true;
        }
        state.set(&chord(), held, ALL_MODIFIERS, at(10)).unwrap();
        let mut delivery = vec![state.current(at(11))];
        delivery.extend(state.key(ESCAPE, false, ALL_MODIFIERS, true, at(12)));
        assert_eq!(
            delivery,
            [
                HotkeyEvent::Pressed { at: at(10) },
                HotkeyEvent::Released { at: at(12) },
            ]
        );
    }

    #[test]
    fn capture_start_end_during_hold_preserves_one_pair_and_original_press_time() {
        let mut state = watching();
        press_modifiers(&mut state);
        assert_eq!(
            state.key(ESCAPE, true, ALL_MODIFIERS, true, at(4)),
            Some(HotkeyEvent::Pressed { at: at(4) })
        );
        // Capture phases own no chord state. The native hook passes these same physical events
        // before capture suppression; neither phase change reconfigures or replaces the matcher.
        for _capturing in [true, false, true, false] {
            assert_eq!(state.key(ESCAPE, true, ALL_MODIFIERS, true, at(5)), None);
            assert_eq!(state.current(at(6)), HotkeyEvent::Pressed { at: at(4) });
        }
        assert_eq!(
            state.key(ESCAPE, false, ALL_MODIFIERS, true, at(7)),
            Some(HotkeyEvent::Released { at: at(7) })
        );
        assert_eq!(state.key(ESCAPE, false, ALL_MODIFIERS, true, at(8)), None);
    }
}
