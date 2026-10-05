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
    input::{LockKeys, ScrollDelta},
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

/// `[E]` VIRTUALDESK absolute endpoints are 0..65535, with negative physical origins supported.
/// <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-mouseinput>
/// `[P]` v120 already retains fractional detents; pixels are not added or guessed into wheel units.
pub fn scroll_packets(delta: ScrollDelta) -> Vec<Packet> {
    let mut packets = Vec::new();
    if delta.v120_x != 0 {
        packets.push(Packet::Wheel {
            horizontal: true,
            v120: delta.v120_x,
        });
    }
    if delta.v120_y != 0 || packets.is_empty() {
        packets.push(Packet::Wheel {
            horizontal: false,
            v120: delta.v120_y,
        });
    }
    packets
}

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
    let info = layout
        .displays
        .iter()
        .find(|d| d.id == display)
        .ok_or(PlatformError::NotFound)?;
    let probe = probes
        .iter()
        .find(|p| !p.twin && ids.assign(&p.device_path).ok() == Some(display))
        .ok_or(PlatformError::NotFound)?;
    let device = info
        .geometry
        .logical_to_device(info.geometry.device_to_logical(point));
    let size = info.geometry.pixel_size;
    if device.x < 0.0
        || device.y < 0.0
        || device.x >= f64::from(size.width)
        || device.y >= f64::from(size.height)
    {
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
