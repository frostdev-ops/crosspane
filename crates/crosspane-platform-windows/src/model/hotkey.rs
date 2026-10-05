//! Raw keyboard chord observation. Device provenance and async-state seeding are observations,
//! not proof that input was physical; see the native adapter's release-only contract.

use crosspane_platform::{Chord, HotkeyEvent, PlatformError};
use crosspane_types::hid::{HidUsage, ScanPrefix, WinScancode, hid_to_windows, windows_to_hid};
use crosspane_types::time::MonoTime;
use std::{collections::BTreeSet, fmt};

use super::hook::ChordTracker;

/// Only the fields needed for chord observation; never format keyboard reports for logs.
pub struct RawKey {
    pub device: usize,
    pub make_code: u16,
    pub flags: u16,
}

impl fmt::Debug for RawKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RawKey { .. }")
    }
}

/// The caller supplies a bounded registration inventory. No keyboard entry is admissible at start.
pub fn registration_available(keyboard_targets: &[usize]) -> bool {
    keyboard_targets.is_empty()
}

/// Exactly our target must remain the process's keyboard recipient.
pub fn registration_owned(keyboard_targets: &[usize], target: usize) -> bool {
    target != 0 && keyboard_targets == [target]
}

#[derive(Default)]
pub struct HotkeyModel {
    tracker: ChordTracker,
    chord: Option<Chord>,
    pressed: bool,
    subscribed: bool,
    lost: bool,
    held: BTreeSet<(usize, HidUsage)>,
    seed: BTreeSet<HidUsage>,
}

impl fmt::Debug for HotkeyModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HotkeyModel")
            .field("lost", &self.lost)
            .finish_non_exhaustive()
    }
}

impl HotkeyModel {
    pub fn chord(&self) -> Option<&Chord> {
        self.chord.as_ref()
    }

    pub fn available(&self) -> Result<(), PlatformError> {
        if self.lost {
            Err(PlatformError::Unsupported(
                "raw keyboard observation unavailable",
            ))
        } else {
            Ok(())
        }
    }

    pub fn configure(
        &mut self,
        chord: &Chord,
        down: &[HidUsage],
        at: MonoTime,
    ) -> Result<Vec<HotkeyEvent>, PlatformError> {
        self.available()?;
        if std::iter::once(&chord.key)
            .chain(&chord.modifiers)
            .any(|key| hid_to_windows(*key).is_none())
        {
            return Err(PlatformError::Unsupported("unmapped hotkey chord"));
        }
        if self.chord.as_ref() == Some(chord) {
            return Ok(Vec::new());
        }
        // The initial async snapshot has no device identity. Transfer that provisional holding
        // to the first real report for each usage; later devices have independent holdings.
        let mut events = Vec::new();
        for usage in down
            .iter()
            .copied()
            .filter(|usage| *usage == chord.key || chord.modifiers.contains(usage))
        {
            if !self.is_down(usage) {
                self.seed.insert(usage);
                if let Some(event) = self.tracker.on_key(usage, true, false, at) {
                    events.push(event);
                }
            }
        }
        self.chord = Some(chord.clone());
        events.extend(self.tracker.set_chord(chord, at));
        self.remember(&events);
        Ok(events)
    }

    pub fn subscribe(&mut self, at: MonoTime) -> Result<HotkeyEvent, PlatformError> {
        self.available()?;
        if self.chord.is_none() {
            return Err(PlatformError::Unsupported("hotkey chord not configured"));
        }
        if self.subscribed {
            return Err(PlatformError::Backend(
                "GlobalHotkeys::subscribe called twice".into(),
            ));
        }
        self.subscribed = true;
        Ok(if self.pressed {
            HotkeyEvent::Pressed { at }
        } else {
            HotkeyEvent::Released { at }
        })
    }

    /// A fresh snapshot immediately before the first subscription, with no provenance claim.
    /// This never resets an established subscription or a pair delivered across capture.
    pub fn seed_initial(&mut self, down: &[HidUsage], at: MonoTime) -> Result<(), PlatformError> {
        self.available()?;
        if self.subscribed {
            return Err(PlatformError::Backend(
                "hotkey initial state already delivered".into(),
            ));
        }
        let chord = self
            .chord
            .clone()
            .ok_or(PlatformError::Unsupported("hotkey chord not configured"))?;
        let mut initial = Self::default();
        initial.configure(&chord, down, at)?;
        *self = initial;
        Ok(())
    }

    pub fn raw(&mut self, key: RawKey, at: MonoTime) -> Option<HotkeyEvent> {
        if self.lost
            || key.device == 0
            || key.make_code == 0
            || key.make_code == 0xff
            || key.flags & !7 != 0
            || key.flags & 6 == 6
        {
            return None;
        }
        let usage = windows_to_hid(WinScancode {
            code: u8::try_from(key.make_code).ok()?,
            prefix: if key.flags & 2 != 0 {
                ScanPrefix::E0
            } else if key.flags & 4 != 0 {
                ScanPrefix::E1
            } else {
                ScanPrefix::None
            },
        })?;
        let before = self.is_down(usage);
        self.seed.remove(&usage);
        if key.flags & 1 == 0 {
            self.held.insert((key.device, usage));
        } else {
            self.held.remove(&(key.device, usage));
        }
        // Bound adversarial device identities without inventing a physical release on loss.
        if self.held.len() > 4096 {
            self.lose();
            return None;
        }
        let after = self.is_down(usage);
        let event = if before == after {
            None
        } else {
            self.tracker.on_key(usage, after, false, at)
        };
        if let Some(event) = event {
            self.remember(&[event]);
        }
        event
    }

    fn is_down(&self, usage: HidUsage) -> bool {
        self.seed.contains(&usage) || self.held.iter().any(|(_, held)| *held == usage)
    }

    /// Loss has no physical-event representation in the frozen trait.
    pub fn lose(&mut self) {
        self.lost = true;
    }

    fn remember(&mut self, events: &[HotkeyEvent]) {
        for event in events {
            self.pressed = matches!(event, HotkeyEvent::Pressed { .. });
        }
    }
}
