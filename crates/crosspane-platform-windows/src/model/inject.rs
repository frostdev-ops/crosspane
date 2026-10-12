//! Pure W1.2 submission rules. `[P]` Focus/gate generations cancel ownership before new input.
//! `[P]` P9d admits cleanup ups reaching the new foreground; submitted never means delivered.
//! No token acquisition, window activation or Windows calls occur here.
//!
//! | Observation before non-release input | Decision |
//! | --- | --- |
//! | Closed/changed gate | Cancel repeat, attempt all owed ups, return `Locked` |
//! | Unknown/higher-integrity foreground | Cancel repeat, attempt owed ups, return `SecureInput` |
//! | Changed held foreground identity/generation | Drain both ledgers, refuse this submission |
//! | Fresh matching foreground and open gate | Record possible down before submission |
//! | Any release/recovery | Cancel repeat first; submit ups regardless of the guard |
//! | Failed up | Retain it as owed; never claim delivery |

use super::geometry::{DisplayIds, MonitorProbe, displays};
use super::hook::DragSettlementResult;
use crosspane_platform::{IoGate, PlatformError};
use crosspane_types::{
    geom::PointDevice,
    hid::{HidUsage, MouseButton, ScanPrefix, hid_to_windows},
    id::DisplayId,
    input::{LockKeys, ScrollDelta, ScrollPhase},
};
use std::{collections::BTreeSet, sync::Arc};

/// `[P]` P9d uses PID creation time and foreground event generation against reuse and ABA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Foreground {
    pub window: u64,
    pub process: u32,
    pub thread: u32,
    pub born: u64,
    pub generation: u64,
    pub integrity: u32,
}

/// Oldest a monitor table may be for the pointer injector to consult it without a fresh
/// acquisition. Even a younger table is reused only after it matches the live monitors.
pub const MONITOR_CACHE_MAX_AGE_MS: u64 = 1000;

/// Pure age rule: a table is consultable only if it has rows and was read strictly less than
/// `MONITOR_CACHE_MAX_AGE_MS` ago. `None` means never read (or invalidated), which is never fresh.
pub fn monitor_cache_fresh(rows: usize, age_ms: Option<u64>) -> bool {
    rows != 0 && age_ms.is_some_and(|age| age < MONITOR_CACHE_MAX_AGE_MS)
}

/// Reuse rule for the cached monitor table. `validate` (the live comparison and the move
/// resolution against the cached rows) runs only for a fresh table, so a stale, empty or
/// invalidated table never reaches it, and a table that fails it is never reused.
pub fn reuse_monitors(rows: usize, age_ms: Option<u64>, validate: impl FnOnce() -> bool) -> bool {
    monitor_cache_fresh(rows, age_ms) && validate()
}

/// The identity under which an admitted foreground decision may be reused. Any difference in the
/// window, process, thread or foreground-event generation forces the full process and token check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionKey {
    pub window: u64,
    pub process: u32,
    pub thread: u32,
    pub generation: u64,
}

/// The last foreground that passed the full process and token check, with the process creation
/// time and integrity it was admitted with. It holds an admission only: a refusal or a changed
/// key empties it, so a refusal is never reused and the full check always follows a key change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdmissionCache {
    admitted: Option<Foreground>,
}
impl AdmissionCache {
    /// The cached admission, only for exactly this key.
    pub fn reuse(&self, key: AdmissionKey) -> Option<Foreground> {
        self.admitted.filter(|f| {
            f.window != 0
                && f.process != 0
                && f.thread != 0
                && f.born != 0
                && (f.window, f.process, f.thread, f.generation)
                    == (key.window, key.process, key.thread, key.generation)
        })
    }
    /// Remembers a foreground that has just passed the full check and its post-check.
    pub fn admit(&mut self, foreground: Foreground) {
        self.admitted = Some(foreground);
    }
    /// Forgets the admission; the next observation runs the full check.
    pub fn invalidate(&mut self) {
        self.admitted = None;
    }
}

/// `[E]` Native packets contain physical scancodes, never text or virtual keys.
/// <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-keybdinput>
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet {
    Key {
        scan: u16,
        extended: bool,
        down: bool,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Move {
        x: i32,
        y: i32,
    },
    Wheel {
        horizontal: bool,
        v120: i32,
    },
}

/// Native boundary; a failed submit may already have submitted the packet.
pub trait InjectionPort: Send {
    fn foreground(&mut self) -> Result<Foreground, PlatformError>;
    fn submit(&mut self, packet: Packet) -> Result<(), PlatformError>;
    fn locks(&mut self) -> Result<LockKeys, PlatformError>;
}

/// The dedicated local-up path distinguishes exact native zero from generic uncertainty.
#[allow(dead_code)] // Native-only consumer; exact source is included by pure contract tests.
pub(crate) struct LocalSettlementOutcome {
    pub result: DragSettlementResult,
    pub error: Option<PlatformError>,
}

/// `[E]` Read-only keyboard settings: delay 0..3 and speed 0..31.
/// <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-systemparametersinfow>
/// `[P]` Linear speed interpolation is an approximation (Windows documents hardware variation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepeatSettings {
    pub delay_ms: u64,
    pub interval_ms: u64,
}
impl RepeatSettings {
    pub fn new(delay: u32, speed: u32) -> Result<Self, PlatformError> {
        if delay > 3 || speed > 31 {
            return Err(PlatformError::Unsupported("keyboard repeat settings"));
        }
        Ok(Self {
            delay_ms: 250 * u64::from(delay + 1),
            interval_ms: (1000.0 / (2.5 + 27.5 * f64::from(speed) / 31.0)).ceil() as u64,
        })
    }
}

/// `[P]` Pixel-only scrolling uses a 20 logical-pixel line/character heuristic.
/// This makes continuous sources usable, but does not promise native pixel parity.
/// Zero, page-scroll and unreadable OS settings conservatively use three lines/characters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollSettings {
    units_per_pixel: [f64; 2],
}
impl ScrollSettings {
    pub fn new(chars: Option<u32>, lines: Option<u32>) -> Self {
        let scale = |setting: Option<u32>| {
            let count = setting.filter(|n| *n != 0 && *n != u32::MAX).unwrap_or(3);
            120.0 / (f64::from(count) * 20.0)
        };
        Self {
            units_per_pixel: [scale(chars), scale(lines)],
        }
    }
}
impl Default for ScrollSettings {
    fn default() -> Self {
        Self::new(None, None)
    }
}

#[derive(Default)]
struct ScrollAccumulator {
    settings: ScrollSettings,
    remainder: [f64; 2],
    direction: [i8; 2],
}
impl ScrollAccumulator {
    fn reset(&mut self) {
        self.remainder = [0.0; 2];
        self.direction = [0; 2];
    }
    fn packets(&mut self, delta: ScrollDelta) -> Result<Vec<Packet>, PlatformError> {
        let pixels = delta.pixels.map(|p| [p.x, p.y]);
        if pixels.is_some_and(|p| p.iter().any(|n| !n.is_finite())) {
            self.reset();
            return Err(PlatformError::Unsupported("nonfinite scroll delta"));
        }
        let ending = matches!(
            delta.phase,
            ScrollPhase::Ended | ScrollPhase::Cancelled | ScrollPhase::MomentumEnded
        );
        if ending || delta.phase == ScrollPhase::Began {
            self.reset();
        }
        let mut wheel = [delta.v120_x, delta.v120_y];
        for (axis, units) in wheel.iter_mut().enumerate() {
            if *units != 0 {
                // A v120 source is authoritative for this axis; never add pixels or debt.
                self.remainder[axis] = 0.0;
                self.direction[axis] = units.signum() as i8;
            } else if let Some(pixels) = pixels {
                let value = pixels[axis];
                let direction = if value > 0.0 {
                    1
                } else if value < 0.0 {
                    -1
                } else {
                    0
                };
                if direction != 0 && direction != self.direction[axis] {
                    self.remainder[axis] = 0.0;
                    self.direction[axis] = direction;
                }
                let total = value * self.settings.units_per_pixel[axis] + self.remainder[axis];
                let whole = total
                    .trunc()
                    .clamp(f64::from(i32::MIN), f64::from(i32::MAX));
                *units = whole as i32;
                self.remainder[axis] = if total < f64::from(i32::MIN) || total > f64::from(i32::MAX)
                {
                    // A clamped emission never leaves an unbounded catch-up backlog.
                    0.0
                } else {
                    total - whole
                };
            }
        }
        for (axis, stopped) in [delta.stop_x, delta.stop_y].into_iter().enumerate() {
            if stopped {
                self.remainder[axis] = 0.0;
                self.direction[axis] = 0;
            }
        }
        if ending {
            self.reset();
        }
        Ok(scroll_packets(ScrollDelta {
            v120_x: wheel[0],
            v120_y: wheel[1],
            pixels: None,
            ..delta
        }))
    }
}

pub fn key_packet(usage: HidUsage, down: bool) -> Result<Packet, PlatformError> {
    let scan = hid_to_windows(usage).ok_or(PlatformError::Unsupported("unmapped physical key"))?;
    if scan.prefix == ScanPrefix::E1 {
        return Err(PlatformError::Unsupported("E1 physical key"));
    }
    Ok(Packet::Key {
        scan: u16::from(scan.code),
        extended: scan.prefix == ScanPrefix::E0,
        down,
    })
}

/// Shared source model; failed releases stay owed, including lock-toggle cleanup.
pub struct Driver<P: InjectionPort> {
    port: P,
    gate: Arc<IoGate>,
    own_integrity: u32,
    keys: BTreeSet<HidUsage>,
    buttons: BTreeSet<MouseButton>,
    toggle_ups: BTreeSet<HidUsage>,
    focus: Option<(Foreground, u64)>,
    settings: RepeatSettings,
    repeat: Option<(HidUsage, u64)>,
    scroll: ScrollAccumulator,
    scroll_focus: Option<(Foreground, u64)>,
}
impl<P: InjectionPort> std::fmt::Debug for Driver<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Driver(..)")
    }
}
impl<P: InjectionPort> Driver<P> {
    pub fn new(port: P, gate: Arc<IoGate>, own_integrity: u32, settings: RepeatSettings) -> Self {
        Self {
            port,
            gate,
            own_integrity,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            toggle_ups: BTreeSet::new(),
            focus: None,
            settings,
            repeat: None,
            scroll: ScrollAccumulator::default(),
            scroll_focus: None,
        }
    }
    pub fn port(&self) -> &P {
        &self.port
    }
    pub fn port_mut(&mut self) -> &mut P {
        &mut self.port
    }
    pub fn gate(&self) -> &Arc<IoGate> {
        &self.gate
    }
    pub fn held_keys(&self) -> &BTreeSet<HidUsage> {
        &self.keys
    }
    pub fn held_buttons(&self) -> &BTreeSet<MouseButton> {
        &self.buttons
    }
    pub fn repeating(&self) -> bool {
        self.repeat.is_some()
    }
    pub fn cancel_repeat(&mut self) {
        self.repeat = None;
    }
    pub fn set_scroll_settings(&mut self, settings: ScrollSettings) {
        self.reset_scroll();
        self.scroll.settings = settings;
    }
    pub fn reset_scroll(&mut self) {
        self.scroll.reset();
        self.scroll_focus = None;
    }

    /// Target-bound LOCAL settlement, distinct from deliberately permissive ordinary cleanup.
    /// No injected down/up ledger is added: its original down belongs to the capture seat.
    #[allow(dead_code)]
    pub(crate) fn settle_local_button(
        &mut self,
        expected: Foreground,
        reserve: impl FnOnce() -> Result<(), PlatformError>,
        send: impl FnOnce(&mut P) -> LocalSettlementOutcome,
    ) -> Result<LocalSettlementOutcome, PlatformError> {
        self.cancel_repeat();
        let epoch = self.gate.epoch();
        self.check_local_target(expected)?; // Refusal here has no tail reservation.
        if let Err(e) = reserve().and_then(|()| {
            self.check_local_target(expected)?;
            if self.gate.epoch() != epoch {
                return Err(PlatformError::Locked);
            }
            Ok(())
        }) {
            return Ok(LocalSettlementOutcome {
                result: DragSettlementResult::KnownZero,
                error: Some(e),
            });
        }
        // The adapter returns exact SendInput(1) count or terminal uncertainty. Generic Err is
        // not promoted to zero; unwinding is caught outside this method with the tail protected.
        Ok(send(&mut self.port))
    }

    fn check_local_target(&mut self, expected: Foreground) -> Result<(), PlatformError> {
        let epoch = self.gate.epoch();
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if !self.buttons.is_empty() {
            return Err(PlatformError::Backend(
                "injected pointer obligation outstanding".into(),
            ));
        }
        let observed = self
            .port
            .foreground()
            .map_err(|_| PlatformError::SecureInput)?;
        if observed != expected
            || observed.window == 0
            || observed.process == 0
            || observed.thread == 0
            || observed.born == 0
            || observed.integrity > self.own_integrity
            || self
                .focus
                .is_some_and(|(owned, old_epoch)| owned != observed || old_epoch != epoch)
        {
            return Err(PlatformError::SecureInput);
        }
        if !self.gate.is_open() || self.gate.epoch() != epoch {
            return Err(PlatformError::Locked);
        }
        Ok(())
    }

    fn empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty() && self.toggle_ups.is_empty()
    }
    fn clear_focus_if_empty(&mut self) {
        if self.empty() {
            self.focus = None;
        }
    }

    fn guard(&mut self) -> Result<Foreground, PlatformError> {
        let epoch = self.gate.epoch();
        if !self.gate.is_open() || self.focus.is_some_and(|(_, old)| old != epoch) {
            self.cancel_repeat();
            let _ = self.release_everything();
            return Err(PlatformError::Locked);
        }
        let foreground = match self.port.foreground() {
            Ok(f)
                if f.window != 0
                    && f.process != 0
                    && f.thread != 0
                    && f.integrity <= self.own_integrity =>
            {
                f
            }
            _ => {
                self.cancel_repeat();
                let _ = self.release_everything();
                return Err(PlatformError::SecureInput);
            }
        };
        if self.focus.is_some_and(|(old, _)| old != foreground) {
            self.cancel_repeat();
            let _ = self.release_everything();
            return Err(PlatformError::SecureInput);
        }
        // Re-read the gate after native observation, immediately before submission.
        if !self.gate.is_open() || self.gate.epoch() != epoch {
            self.cancel_repeat();
            let _ = self.release_everything();
            return Err(PlatformError::Locked);
        }
        Ok(foreground)
    }

    pub fn key(&mut self, usage: HidUsage, down: bool, now_ms: u64) -> Result<(), PlatformError> {
        if !down {
            self.cancel_repeat();
        }
        let packet = key_packet(usage, down)?;
        if !down {
            self.cancel_repeat();
            if !self.toggle_ups.contains(&usage) {
                self.keys.insert(usage);
            }
            self.port.submit(packet)?;
            self.keys.remove(&usage);
            self.toggle_ups.remove(&usage);
            self.clear_focus_if_empty();
            return Ok(());
        }
        let foreground = self.guard()?;
        self.focus = Some((foreground, self.gate.epoch()));
        self.keys.insert(usage);
        let result = self.port.submit(packet);
        if result.is_err() {
            self.cancel_repeat();
        } else if !usage.is_modifier() && !matches!(usage.id, 0x39 | 0x47 | 0x53) {
            self.repeat = Some((usage, now_ms.saturating_add(self.settings.delay_ms)));
        }
        result
    }

    pub fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        if !down {
            self.cancel_repeat();
        }
        if !(1..=5).contains(&button.0) {
            return Err(PlatformError::Unsupported("pointer button"));
        }
        if !down {
            self.cancel_repeat();
            self.buttons.insert(button);
            self.port.submit(Packet::Button { button, down })?;
            self.buttons.remove(&button);
            self.clear_focus_if_empty();
            return Ok(());
        }
        let foreground = self.guard()?;
        self.focus = Some((foreground, self.gate.epoch()));
        self.buttons.insert(button);
        self.port.submit(Packet::Button { button, down })
    }

    pub fn submit_guarded(&mut self, packet: Packet) -> Result<(), PlatformError> {
        if !matches!(packet, Packet::Move { .. } | Packet::Wheel { .. }) {
            return Err(PlatformError::Unsupported("unowned guarded packet"));
        }
        self.guard()?;
        self.port.submit(packet)
    }

    pub fn scroll(&mut self, delta: ScrollDelta) -> Result<(), PlatformError> {
        let foreground = self.guard()?;
        let context = (foreground, self.gate.epoch());
        if self.scroll_focus != Some(context) {
            self.reset_scroll();
            self.scroll_focus = Some(context);
        }
        let packets = match self.scroll.packets(delta) {
            Ok(packets) => packets,
            Err(error) => {
                self.reset_scroll();
                return Err(error);
            }
        };
        for packet in packets {
            // Each axis is separately guarded, including a fence between the two submissions.
            if (self.guard()?, self.gate.epoch()) != context {
                self.reset_scroll();
                return Err(PlatformError::SecureInput);
            }
            if let Err(error) = self.port.submit(packet) {
                self.reset_scroll();
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn repeat(&mut self, now_ms: u64) -> Result<(), PlatformError> {
        let Some((usage, due)) = self.repeat else {
            return Ok(());
        };
        // Check on every timer turn, even before due: closed/changed gates cancel immediately.
        self.guard()?;
        if now_ms >= due {
            let result = self.port.submit(key_packet(usage, true)?);
            if result.is_err() {
                self.cancel_repeat();
                return result;
            }
            self.repeat = Some((usage, now_ms.saturating_add(self.settings.interval_ms)));
        }
        Ok(())
    }

    pub fn release_keys(&mut self) -> Result<(), PlatformError> {
        self.cancel_repeat();
        self.reset_scroll();
        let keys: BTreeSet<_> = self.keys.union(&self.toggle_ups).copied().collect();
        let mut failure = None;
        for usage in keys {
            if let Err(e) = self.key(usage, false, 0) {
                failure = Some(e);
            }
        }
        failure.map_or(Ok(()), Err)
    }
    pub fn release_buttons(&mut self) -> Result<(), PlatformError> {
        self.cancel_repeat();
        self.reset_scroll();
        let buttons: Vec<_> = self.buttons.iter().copied().collect();
        let mut failure = None;
        for button in buttons {
            if let Err(e) = self.button(button, false) {
                failure = Some(e);
            }
        }
        failure.map_or(Ok(()), Err)
    }
    pub fn release_everything(&mut self) -> Result<(), PlatformError> {
        let keys = self.release_keys();
        let buttons = self.release_buttons();
        keys.and(buttons)
    }
    pub fn recover_keys(&mut self, keys: &[HidUsage]) -> Result<(), PlatformError> {
        self.cancel_repeat();
        self.reset_scroll();
        let mut failure = None;
        for &usage in keys {
            if let Err(e) = key_packet(usage, false) {
                failure = Some(e);
                continue;
            }
            self.keys.insert(usage);
            if let Err(e) = self.key(usage, false, 0) {
                failure = Some(e);
            }
        }
        failure.map_or(Ok(()), Err)
    }
    pub fn recover_buttons(&mut self, buttons: &[MouseButton]) -> Result<(), PlatformError> {
        self.cancel_repeat();
        self.reset_scroll();
        let mut failure = None;
        for &button in buttons {
            if !(1..=5).contains(&button.0) {
                failure = Some(PlatformError::Unsupported("pointer button"));
                continue;
            }
            self.buttons.insert(button);
            if let Err(e) = self.button(button, false) {
                failure = Some(e);
            }
        }
        failure.map_or(Ok(()), Err)
    }
    pub fn locks(&mut self) -> Result<LockKeys, PlatformError> {
        self.port.locks()
    }
    pub fn set_locks(&mut self, wanted: LockKeys) -> Result<(), PlatformError> {
        self.cancel_repeat();
        let actual = self.port.locks()?;
        for (id, desired, current) in [
            (0x39, wanted.caps_lock, actual.caps_lock),
            (0x53, wanted.num_lock, actual.num_lock),
            (0x47, wanted.scroll_lock, actual.scroll_lock),
        ] {
            let Some(desired) = desired else {
                continue;
            };
            let current = current.ok_or(PlatformError::SecureInput)?;
            if desired == current {
                continue;
            }
            let usage = HidUsage::keyboard(id);
            if self.keys.contains(&usage) {
                return Err(PlatformError::Unsupported("toggle key held"));
            }
            let foreground = self.guard()?;
            self.focus = Some((foreground, self.gate.epoch()));
            self.toggle_ups.insert(usage);
            let down = self.port.submit(key_packet(usage, true)?);
            let up = self.key(usage, false, 0);
            down.and(up)?;
        }
        Ok(())
    }
}

/// `[E]` Nonzero v120 values retain fractional detents; zero does not submit native input.
/// <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-mouseinput>
pub fn scroll_packets(delta: ScrollDelta) -> Vec<Packet> {
    let mut packets = Vec::new();
    if delta.v120_x != 0 {
        packets.push(Packet::Wheel {
            horizontal: true,
            v120: delta.v120_x,
        });
    }
    if delta.v120_y != 0 {
        packets.push(Packet::Wheel {
            horizontal: false,
            v120: delta.v120_y,
        });
    }
    packets
}

/// A display with a published `DisplayInfo` maps through its logical geometry. A twin resolves
/// only when exactly one twin probe assigns to `display`; its point is taken as its own pixels.
pub fn absolute_move(
    probes: &[MonitorProbe],
    ids: &mut DisplayIds,
    display: DisplayId,
    point: PointDevice,
) -> Result<(Packet, MonitorProbe), PlatformError> {
    if probes.is_empty() || probes.len() > 32 || !point.x.is_finite() || !point.y.is_finite() {
        return Err(PlatformError::NotFound);
    }
    let layout = displays(probes, ids).map_err(|_| PlatformError::NotFound)?;
    let (device, (width, height), probe) = match layout.displays.iter().find(|d| d.id == display) {
        Some(info) => {
            let probe = probes
                .iter()
                .find(|p| !p.twin && ids.assign(&p.device_path).ok() == Some(display))
                .ok_or(PlatformError::NotFound)?;
            let device = info
                .geometry
                .logical_to_device(info.geometry.device_to_logical(point));
            let size = info.geometry.pixel_size;
            (
                device,
                (f64::from(size.width), f64::from(size.height)),
                probe,
            )
        }
        None => {
            // A twin has no published DisplayInfo, so it has no scale or origin. The lookup runs
            // on a clone, so a refused or ambiguous lookup never changes the allocator.
            let mut scratch = ids.clone();
            let mut matching = probes.iter().filter(|p| {
                p.twin
                    && !p.device_path.is_empty()
                    && scratch.assign(&p.device_path).ok() == Some(display)
            });
            let probe = matching.next().ok_or(PlatformError::NotFound)?;
            if matching.next().is_some() {
                return Err(PlatformError::NotFound);
            }
            let rc = probe.rc_monitor;
            if rc[2] <= rc[0] || rc[3] <= rc[1] {
                return Err(PlatformError::NotFound);
            }
            let size = (
                f64::from(rc[2]) - f64::from(rc[0]),
                f64::from(rc[3]) - f64::from(rc[1]),
            );
            (point, size, probe)
        }
    };
    if device.x < 0.0 || device.y < 0.0 || device.x >= width || device.y >= height {
        return Err(PlatformError::NotFound);
    }
    let left = probes
        .iter()
        .map(|p| p.rc_monitor[0])
        .min()
        .ok_or(PlatformError::NotFound)?;
    let top = probes
        .iter()
        .map(|p| p.rc_monitor[1])
        .min()
        .ok_or(PlatformError::NotFound)?;
    let right = probes
        .iter()
        .map(|p| p.rc_monitor[2])
        .max()
        .ok_or(PlatformError::NotFound)?;
    let bottom = probes
        .iter()
        .map(|p| p.rc_monitor[3])
        .max()
        .ok_or(PlatformError::NotFound)?;
    let normalize = |value: f64, start: i32, end: i32| -> Result<i32, PlatformError> {
        let span = i64::from(end) - i64::from(start) - 1;
        if span <= 0 {
            return Err(PlatformError::NotFound);
        }
        Ok(((value - f64::from(start)) * 65535.0 / span as f64)
            .round()
            .clamp(0.0, 65535.0) as i32)
    };
    Ok((
        Packet::Move {
            x: normalize(f64::from(probe.rc_monitor[0]) + device.x, left, right)?,
            y: normalize(f64::from(probe.rc_monitor[1]) + device.y, top, bottom)?,
        },
        probe.clone(),
    ))
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::cell::Cell;

    fn admitted(window: u64, process: u32, thread: u32, generation: u64) -> Foreground {
        Foreground {
            window,
            process,
            thread,
            born: 77,
            generation,
            integrity: 0x2000,
        }
    }
    fn key(window: u64, process: u32, thread: u32, generation: u64) -> AdmissionKey {
        AdmissionKey {
            window,
            process,
            thread,
            generation,
        }
    }

    #[test]
    fn monitor_cache_age_rule_is_strict_and_needs_rows_and_a_read() {
        assert_eq!(MONITOR_CACHE_MAX_AGE_MS, 1000);
        assert!(monitor_cache_fresh(1, Some(0)));
        assert!(monitor_cache_fresh(3, Some(MONITOR_CACHE_MAX_AGE_MS - 1)));
        assert!(!monitor_cache_fresh(3, Some(MONITOR_CACHE_MAX_AGE_MS)));
        assert!(!monitor_cache_fresh(3, Some(u64::MAX)));
        assert!(!monitor_cache_fresh(0, Some(0)));
        assert!(!monitor_cache_fresh(3, None));
    }

    #[test]
    fn monitor_reuse_validates_only_a_fresh_table_and_needs_the_validation_to_pass() {
        let runs = Cell::new(0);
        let validate = |passes: bool| {
            let runs = &runs;
            move || {
                runs.set(runs.get() + 1);
                passes
            }
        };
        // Stale, empty and never-read tables never reach the live validation.
        assert!(!reuse_monitors(
            2,
            Some(MONITOR_CACHE_MAX_AGE_MS),
            validate(true)
        ));
        assert!(!reuse_monitors(0, Some(1), validate(true)));
        assert!(!reuse_monitors(2, None, validate(true)));
        assert_eq!(runs.get(), 0);
        // A fresh table is reused only when the validation passes, and is validated once.
        assert!(reuse_monitors(2, Some(1), validate(true)));
        assert_eq!(runs.get(), 1);
        assert!(!reuse_monitors(2, Some(1), validate(false)));
        assert_eq!(runs.get(), 2);
    }

    #[test]
    fn admission_is_reused_only_for_exactly_the_admitted_key() {
        let mut cache = AdmissionCache::default();
        assert_eq!(cache.reuse(key(1, 2, 3, 4)), None);
        cache.admit(admitted(1, 2, 3, 4));
        assert_eq!(cache.reuse(key(1, 2, 3, 4)), Some(admitted(1, 2, 3, 4)));
        // Each key field changes the decision: a new window, pid, thread or event generation.
        for changed in [
            key(9, 2, 3, 4),
            key(1, 9, 3, 4),
            key(1, 2, 9, 4),
            key(1, 2, 3, 9),
        ] {
            assert_eq!(cache.reuse(changed), None, "{changed:?}");
        }
        // A miss does not forget the admission; the caller invalidates before the full check.
        assert_eq!(cache.reuse(key(1, 2, 3, 4)), Some(admitted(1, 2, 3, 4)));
    }

    #[test]
    fn admission_invalidate_and_replacement_drop_the_old_identity() {
        let mut cache = AdmissionCache::default();
        cache.admit(admitted(1, 2, 3, 4));
        cache.invalidate();
        assert_eq!(cache.reuse(key(1, 2, 3, 4)), None);
        cache.admit(admitted(1, 2, 3, 4));
        cache.admit(admitted(5, 6, 7, 8));
        assert_eq!(cache.reuse(key(1, 2, 3, 4)), None);
        assert_eq!(cache.reuse(key(5, 6, 7, 8)), Some(admitted(5, 6, 7, 8)));
    }

    #[test]
    fn admission_never_reuses_an_incomplete_identity() {
        for (window, process, thread, born) in
            [(0, 2, 3, 77), (1, 0, 3, 77), (1, 2, 0, 77), (1, 2, 3, 0)]
        {
            let mut cache = AdmissionCache::default();
            cache.admit(Foreground {
                window,
                process,
                thread,
                born,
                generation: 4,
                integrity: 0x2000,
            });
            assert_eq!(cache.reuse(key(window, process, thread, 4)), None);
        }
    }
}
