//! What this source holds down on its EIS devices (pure bookkeeping).
//!
//! An entry is made *before* the request that presses is queued, so a down that may have been
//! sent always counts as held, and is removed only once its release is queued. The compositor
//! drops the logical state of a device that pauses or goes away, so the owner of the ledger
//! forgets that device then. The ledger dies with its connection.

use std::collections::BTreeMap;

/// A device of the current connection, numbered by the worker as it is announced.
pub(super) type DevId = u64;

/// Which scroll axes have an unfinished smooth gesture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Axes {
    pub x: bool,
    pub y: bool,
}

impl Axes {
    pub(super) fn any(self) -> bool {
        self.x || self.y
    }

    fn union(self, other: Axes) -> Axes {
        Axes {
            x: self.x || other.x,
            y: self.y || other.y,
        }
    }

    fn without(self, other: Axes) -> Axes {
        Axes {
            x: self.x && !other.x,
            y: self.y && !other.y,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Ledger {
    /// evdev key code to the device it is down on.
    keys: BTreeMap<u16, DevId>,
    /// evdev button code to the device it is down on.
    buttons: BTreeMap<u32, DevId>,
    smooth: BTreeMap<DevId, Axes>,
}

impl Ledger {
    /// Record a key down. False when the key was already held.
    pub(super) fn hold_key(&mut self, code: u16, dev: DevId) -> bool {
        self.keys.insert(code, dev).is_none()
    }

    /// Record a key up, returning the device it was held on.
    pub(super) fn drop_key(&mut self, code: u16) -> Option<DevId> {
        self.keys.remove(&code)
    }

    pub(super) fn key_device(&self, code: u16) -> Option<DevId> {
        self.keys.get(&code).copied()
    }

    /// Held keys grouped by device, each in code order.
    pub(super) fn keys_by_device(&self) -> BTreeMap<DevId, Vec<u16>> {
        let mut grouped: BTreeMap<DevId, Vec<u16>> = BTreeMap::new();
        for (&code, &dev) in &self.keys {
            grouped.entry(dev).or_default().push(code);
        }
        grouped
    }

    pub(super) fn hold_button(&mut self, code: u32, dev: DevId) -> bool {
        self.buttons.insert(code, dev).is_none()
    }

    pub(super) fn drop_button(&mut self, code: u32) -> Option<DevId> {
        self.buttons.remove(&code)
    }

    pub(super) fn button_device(&self, code: u32) -> Option<DevId> {
        self.buttons.get(&code).copied()
    }

    pub(super) fn buttons_by_device(&self) -> BTreeMap<DevId, Vec<u32>> {
        let mut grouped: BTreeMap<DevId, Vec<u32>> = BTreeMap::new();
        for (&code, &dev) in &self.buttons {
            grouped.entry(dev).or_default().push(code);
        }
        grouped
    }

    /// A smooth scroll displacement went out on these axes.
    pub(super) fn start_smooth(&mut self, dev: DevId, axes: Axes) {
        if axes.any() {
            let entry = self.smooth.entry(dev).or_default();
            *entry = entry.union(axes);
        }
    }

    /// The gesture on these axes was stopped.
    pub(super) fn end_smooth(&mut self, dev: DevId, axes: Axes) {
        if let Some(entry) = self.smooth.get_mut(&dev) {
            *entry = entry.without(axes);
            if !entry.any() {
                self.smooth.remove(&dev);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn smooth_axes(&self, dev: DevId) -> Axes {
        self.smooth.get(&dev).copied().unwrap_or_default()
    }

    pub(super) fn smooth_devices(&self) -> Vec<(DevId, Axes)> {
        self.smooth
            .iter()
            .map(|(&dev, &axes)| (dev, axes))
            .collect()
    }

    /// The compositor reset (paused) or removed this device: nothing is held on it any more.
    pub(super) fn forget_device(&mut self, dev: DevId) {
        self.keys.retain(|_, d| *d != dev);
        self.buttons.retain(|_, d| *d != dev);
        self.smooth.remove(&dev);
    }

    /// Nothing is held or in progress on this device.
    pub(super) fn is_idle(&self, dev: DevId) -> bool {
        !self.keys.values().any(|d| *d == dev)
            && !self.buttons.values().any(|d| *d == dev)
            && !self.smooth.contains_key(&dev)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty() && self.smooth.is_empty()
    }

    pub(super) fn clear(&mut self) {
        self.keys.clear();
        self.buttons.clear();
        self.smooth.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_held_once_and_released_once() {
        let mut ledger = Ledger::default();
        assert!(ledger.is_empty());
        assert!(ledger.hold_key(30, 1));
        assert!(!ledger.hold_key(30, 1), "a second down is not a new hold");
        assert_eq!(ledger.key_device(30), Some(1));
        assert_eq!(ledger.drop_key(30), Some(1));
        assert_eq!(ledger.drop_key(30), None, "the up is owed only once");
        assert!(ledger.is_empty());
    }

    #[test]
    fn holds_group_by_device_in_code_order() {
        let mut ledger = Ledger::default();
        ledger.hold_key(42, 1);
        ledger.hold_key(30, 1);
        ledger.hold_key(57, 2);
        ledger.hold_button(0x110, 3);
        ledger.hold_button(0x111, 3);
        let keys = ledger.keys_by_device();
        assert_eq!(keys.get(&1), Some(&vec![30, 42]));
        assert_eq!(keys.get(&2), Some(&vec![57]));
        assert_eq!(
            ledger.buttons_by_device().get(&3),
            Some(&vec![0x110, 0x111])
        );
        assert_eq!(ledger.button_device(0x111), Some(3));
        assert_eq!(ledger.drop_button(0x110), Some(3));
        assert_eq!(ledger.drop_button(0x110), None);
    }

    #[test]
    fn forgetting_a_device_drops_only_its_holds() {
        let mut ledger = Ledger::default();
        ledger.hold_key(30, 1);
        ledger.hold_key(31, 2);
        ledger.hold_button(0x110, 1);
        ledger.start_smooth(1, Axes { x: false, y: true });
        ledger.start_smooth(2, Axes { x: true, y: false });
        assert!(!ledger.is_idle(1));
        ledger.forget_device(1);
        assert!(ledger.is_idle(1));
        assert_eq!(ledger.key_device(30), None);
        assert_eq!(ledger.button_device(0x110), None);
        assert_eq!(ledger.smooth_axes(1), Axes::default());
        assert_eq!(ledger.key_device(31), Some(2));
        assert_eq!(ledger.smooth_axes(2), Axes { x: true, y: false });
        assert!(!ledger.is_empty());
        ledger.clear();
        assert!(ledger.is_empty());
    }

    #[test]
    fn smooth_axes_accumulate_and_end_independently() {
        let mut ledger = Ledger::default();
        ledger.start_smooth(5, Axes { x: false, y: true });
        ledger.start_smooth(5, Axes { x: true, y: false });
        assert_eq!(ledger.smooth_axes(5), Axes { x: true, y: true });
        ledger.end_smooth(5, Axes { x: true, y: false });
        assert_eq!(ledger.smooth_axes(5), Axes { x: false, y: true });
        assert_eq!(
            ledger.smooth_devices(),
            vec![(5, Axes { x: false, y: true })]
        );
        ledger.end_smooth(5, Axes { x: false, y: true });
        assert!(ledger.is_empty());
        // Starting nothing records nothing.
        ledger.start_smooth(5, Axes::default());
        assert!(ledger.is_empty());
        // Ending on a device with no gesture is nothing.
        ledger.end_smooth(9, Axes { x: true, y: true });
        assert!(ledger.is_empty());
    }
}
