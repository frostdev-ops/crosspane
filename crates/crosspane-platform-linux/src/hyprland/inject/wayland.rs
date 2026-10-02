//! Thread-confined virtual devices, conservative ownership and bounded Wayland synchronization.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant};

use crosspane_platform::{IoGate, PlatformError};
use crosspane_types::geom::PointDevice;
use crosspane_types::hid::{HidUsage, MouseButton, hid_to_evdev};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{wl_callback, wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use xkbcommon::xkb;

use super::config::{Config, Refresh, Rmlvo, Update, Watcher, snapshot_is_current};
use super::{Action, CALL_BUDGET, Command, InjectedPosition, backend};

const TICK: Duration = Duration::from_millis(10);

#[derive(Default)]
struct Output {
    name: Option<String>,
    width: u32,
    height: u32,
    transform: u32,
}

impl Output {
    fn extent(&self) -> (u32, u32) {
        rotated_extent(self.width, self.height, self.transform)
    }
}

#[derive(Default)]
struct Events {
    globals: BTreeMap<u32, (String, u32)>,
    outputs: BTreeMap<u32, Output>,
    synced: u64,
    outputs_changed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == wl_output::WlOutput::interface().name {
                    state.outputs.insert(name, Output::default());
                    state.outputs_changed = true;
                }
                state.globals.insert(name, (interface, version));
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.globals.remove(&name);
                if state.outputs.remove(&name).is_some() {
                    state.outputs_changed = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for Events {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Late events from a removed global must never resurrect an output.
        let Some(output) = state.outputs.get_mut(id) else {
            return;
        };
        state.outputs_changed = true;
        match event {
            wl_output::Event::Name { name } => output.name = Some(name),
            wl_output::Event::Geometry { transform, .. } => {
                output.transform = match transform {
                    WEnum::Value(value) => value.into(),
                    WEnum::Unknown(value) => value,
                };
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => {
                output.width = u32::try_from(width).unwrap_or_default();
                output.height = u32::try_from(height).unwrap_or_default();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u64> for Events {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        serial: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.synced = state.synced.max(*serial);
    }
}

delegate_noop!(Events: ignore wl_seat::WlSeat);
delegate_noop!(Events: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(Events: ignore ZwpVirtualKeyboardV1);
delegate_noop!(Events: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(Events: ignore ZwlrVirtualPointerV1);

struct Pointer {
    proxy: ZwlrVirtualPointerV1,
    output: u32,
    smooth_x: bool,
    smooth_y: bool,
    remainder_x: i64,
    remainder_y: i64,
}

pub(super) struct Source {
    connection: Connection,
    queue: EventQueue<Events>,
    events: Events,
    registry: wl_registry::WlRegistry,
    seat: Option<wl_seat::WlSeat>,
    pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    outputs: BTreeMap<u32, wl_output::WlOutput>,
    monitors: BTreeMap<String, u32>,
    names: Rmlvo,
    refresh_outputs: bool,
    pointers_open: bool,
    retired: bool,
    gate: Arc<IoGate>,
    injected: Arc<InjectedPosition>,
    keyboard: Option<ZwpVirtualKeyboardV1>,
    xkb: xkb::State,
    pointers: BTreeMap<DisplayId, Pointer>,
    active: Option<DisplayId>,
    held_keys: BTreeSet<u16>,
    xkb_down: BTreeSet<u16>,
    released_keys: BTreeSet<u16>,
    held_buttons: BTreeSet<(DisplayId, u32)>,
    buttons_down: BTreeSet<(DisplayId, u32)>,
    released_buttons: BTreeSet<(DisplayId, u32)>,
    sync_serial: u64,
    /// Paused while a failed configuration refresh leaves the layout and keymap unknown.
    refresh: Refresh,
    /// Counts configuration invalidations: output changes this worker sees, and the keyboard and
    /// output events the watcher's event thread sees. The watcher stamps each read with it, so a
    /// read that began before a change cannot resume a paused worker.
    config_epoch: Arc<AtomicU64>,
}

impl Source {
    pub fn new(
        gate: Arc<IoGate>,
        config: Config,
        injected: Arc<InjectedPosition>,
        deadline: Instant,
    ) -> Result<Self, PlatformError> {
        let connection = connect_display(deadline)?;
        let queue = connection.new_event_queue();
        let registry = connection.display().get_registry(&queue.handle(), ());
        let keymap = compile_keymap(&config.names)?;
        let mut xkb = xkb::State::new(&keymap);
        xkb.update_mask(
            0,
            0,
            lock_mask(&keymap, 0, config.locks),
            0,
            0,
            layout_group(&keymap, &config),
        );
        let mut source = Self {
            connection,
            queue,
            events: Events::default(),
            registry: registry.clone(),
            seat: None,
            pointer_manager: None,
            outputs: BTreeMap::new(),
            monitors: config.monitors.into_iter().collect(),
            names: config.names,
            refresh_outputs: false,
            pointers_open: true,
            retired: false,
            gate,
            injected,
            keyboard: None,
            xkb,
            pointers: BTreeMap::new(),
            active: None,
            held_keys: BTreeSet::new(),
            xkb_down: BTreeSet::new(),
            released_keys: BTreeSet::new(),
            held_buttons: BTreeSet::new(),
            buttons_down: BTreeSet::new(),
            released_buttons: BTreeSet::new(),
            sync_serial: 0,
            refresh: Refresh::default(),
            config_epoch: Arc::new(AtomicU64::new(0)),
        };
        source.sync(deadline)?;
        let qh = source.queue.handle();
        let (name, version) = source.global(wl_seat::WlSeat::interface().name, 4)?;
        let seat: wl_seat::WlSeat = registry.bind(name, version.min(9), &qh, ());
        let (name, _) = source.global(ZwpVirtualKeyboardManagerV1::interface().name, 1)?;
        let manager: ZwpVirtualKeyboardManagerV1 = registry.bind(name, 1, &qh, ());
        let (name, _) = source.global(ZwlrVirtualPointerManagerV1::interface().name, 2)?;
        let pointer_manager: ZwlrVirtualPointerManagerV1 = registry.bind(name, 2, &qh, ());
        source.seat = Some(seat.clone());
        source.pointer_manager = Some(pointer_manager);
        source.maintain_outputs();
        source.sync(deadline)?;
        source.maintain_outputs();
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        upload_keymap(&keyboard, &keymap)?;
        source.keyboard = Some(keyboard);
        source.modifiers();
        source.sync(deadline)?;
        Ok(source)
    }

    fn maintain_outputs(&mut self) {
        let qh = self.queue.handle();
        let removed: Vec<_> = self
            .outputs
            .keys()
            .copied()
            .filter(|id| !self.events.outputs.contains_key(id))
            .collect();
        for id in removed {
            let displays: Vec<_> = self
                .pointers
                .iter()
                .filter_map(|(&display, p)| (p.output == id).then_some(display))
                .collect();
            for display in displays {
                self.retire_pointer(display);
            }
            if let Some(output) = self.outputs.remove(&id) {
                output.release();
            }
        }
        for (&id, (interface, version)) in &self.events.globals {
            if interface == wl_output::WlOutput::interface().name && *version >= 4 {
                self.outputs
                    .entry(id)
                    .or_insert_with(|| self.registry.bind(id, 4, &qh, id));
            }
        }
        // A pointer is bound to the DisplayId that `monitors` gives its output's name. While a
        // refresh has failed that map may be stale, so create no pointers from it; the refresh
        // that resumes the worker creates them. Removal above must still run.
        if self.pointers_open
            && !self.refresh.is_paused()
            && let (Some(seat), Some(manager)) = (&self.seat, &self.pointer_manager)
        {
            for (&id, output) in &self.outputs {
                let Some(info) = self.events.outputs.get(&id) else {
                    continue;
                };
                let Some(display) = info
                    .name
                    .as_ref()
                    .and_then(|name| self.monitors.get(name))
                    .copied()
                    .map(DisplayId)
                else {
                    continue;
                };
                let (width, height) = info.extent();
                if width == 0 || height == 0 {
                    continue;
                }
                self.pointers.entry(display).or_insert_with(|| Pointer {
                    proxy: manager.create_virtual_pointer_with_output(
                        Some(seat),
                        Some(output),
                        &qh,
                        (),
                    ),
                    output: id,
                    smooth_x: false,
                    smooth_y: false,
                    remainder_x: 0,
                    remainder_y: 0,
                });
            }
        }
        if self
            .active
            .is_none_or(|id| !self.pointers.contains_key(&id))
        {
            self.active = self.pointers.keys().next().copied();
        }
        if std::mem::take(&mut self.events.outputs_changed) {
            self.refresh_outputs = true;
            self.config_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn config_epoch(&self) -> Arc<AtomicU64> {
        self.config_epoch.clone()
    }

    /// Destroy one virtual pointer. Retiring a device still owes explicit releases, even with the
    /// gate closed.
    fn retire_pointer(&mut self, display: DisplayId) {
        let Some(pointer) = self.pointers.remove(&display) else {
            return;
        };
        for &(held_display, code) in &self.held_buttons {
            if held_display == display {
                pointer
                    .proxy
                    .button(time_ms(), code, wl_pointer::ButtonState::Released);
                self.released_buttons.insert((display, code));
                self.buttons_down.remove(&(display, code));
            }
        }
        pointer.proxy.frame();
        pointer.proxy.destroy();
        self.retired = true;
    }

    /// Retire every pointer whose DisplayId is no longer what `monitors` says for its output's
    /// name (an output recreated under the same name with a new monitor id keeps no old binding).
    /// `maintain_outputs` binds the right one afterwards.
    fn reconcile_pointers(&mut self) {
        let stale = stale_displays(
            self.pointers.iter().map(|(&display, pointer)| {
                let name = self
                    .events
                    .outputs
                    .get(&pointer.output)
                    .and_then(|info| info.name.as_deref());
                (display, name)
            }),
            &self.monitors,
        );
        for display in stale {
            self.retire_pointer(display);
        }
    }

    /// Check and install a configuration the watcher read. A rejected one changes nothing.
    fn install_config(&mut self, config: Config, deadline: Instant) -> Result<(), PlatformError> {
        // Validate before changing anything, so a rejected configuration leaves no half-applied
        // state behind.
        let keymap = if self.keyboard.is_some() {
            validate_config(&self.names, &config)?
        } else {
            None
        };
        self.monitors = config.monitors.iter().cloned().collect();
        self.reconcile_pointers();
        self.maintain_outputs();
        if self.keyboard.is_none() {
            return self.sync(deadline);
        }
        if let Some(keymap) = keymap {
            let mut state = xkb::State::new(&keymap);
            for &code in &self.xkb_down {
                state.update_key((u32::from(code) + 8).into(), xkb::KeyDirection::Down);
            }
            state.update_mask(
                state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                0,
                lock_mask(&keymap, 0, config.locks),
                0,
                0,
                layout_group(&keymap, &config),
            );
            if let Some(keyboard) = &self.keyboard {
                upload_keymap(keyboard, &keymap)?;
            }
            self.xkb = state;
            self.names = config.names;
            // Not `allowed`: that refuses while paused, and the sync below enforces the deadline.
            if self.gate.is_open() {
                self.modifiers();
            }
        } else {
            let group = layout_group(&self.xkb.get_keymap(), &config);
            if group != self.xkb.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE) {
                self.xkb.update_mask(
                    self.xkb.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                    self.xkb.serialize_mods(xkb::STATE_MODS_LATCHED),
                    self.xkb.serialize_mods(xkb::STATE_MODS_LOCKED),
                    0,
                    0,
                    group,
                );
                if self.gate.is_open() {
                    self.modifiers();
                }
            }
        }
        self.sync(deadline)
    }

    fn global(&self, interface: &str, minimum: u32) -> Result<(u32, u32), PlatformError> {
        self.events
            .globals
            .iter()
            .find_map(|(&name, (kind, version))| {
                (kind == interface && *version >= minimum).then_some((name, *version))
            })
            .ok_or(PlatformError::Unsupported(
                "required virtual-input protocol or seat unavailable",
            ))
    }

    fn allowed(&self, deadline: Instant) -> Result<(), PlatformError> {
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if self.refresh.is_paused() {
            return Err(PlatformError::Timeout);
        }
        Ok(())
    }

    /// Nothing may be injected: the gate is closed, or a failed refresh left the configuration
    /// unknown. The two are independent and both must be clear.
    fn blocked(&self) -> bool {
        !self.gate.is_open() || self.refresh.is_paused()
    }

    fn modifiers(&self) {
        if let Some(keyboard) = &self.keyboard {
            keyboard.modifiers(
                self.xkb.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                self.xkb.serialize_mods(xkb::STATE_MODS_LATCHED),
                self.xkb.serialize_mods(xkb::STATE_MODS_LOCKED),
                self.xkb.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
            );
        }
    }

    fn key(&mut self, evdev: u16, down: bool, deadline: Instant) -> Result<(), PlatformError> {
        if self.keyboard.is_none() {
            return Err(backend("virtual keyboard closed"));
        }
        if let Some(keyboard) = &self.keyboard {
            // Check at the actual submission, not only when the command was queued.
            if down {
                self.allowed(deadline)?;
                // Only a down that reaches submission belongs in the release ledger.
                self.held_keys.insert(evdev);
                self.released_keys.remove(&evdev);
            }
            keyboard.key(time_ms(), u32::from(evdev), u32::from(down));
        }
        let changed = if down {
            self.xkb_down.insert(evdev)
        } else {
            self.xkb_down.remove(&evdev)
        };
        if changed {
            self.xkb.update_key(
                (u32::from(evdev) + 8).into(),
                if down {
                    xkb::KeyDirection::Down
                } else {
                    xkb::KeyDirection::Up
                },
            );
        }
        if !down && self.held_keys.contains(&evdev) {
            self.released_keys.insert(evdev);
        }
        self.modifiers();
        Ok(())
    }

    fn release_keys(&mut self) {
        for code in self.held_keys.clone() {
            let _ = self.key(code, false, Instant::now());
        }
    }

    fn button_on(
        &mut self,
        display: DisplayId,
        code: u32,
        down: bool,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        let pointer = self.pointers.get(&display).ok_or(PlatformError::NotFound)?;
        if down {
            self.allowed(deadline)?;
            self.held_buttons.insert((display, code));
            self.released_buttons.remove(&(display, code));
        }
        pointer.proxy.button(
            time_ms(),
            code,
            if down {
                wl_pointer::ButtonState::Pressed
            } else {
                wl_pointer::ButtonState::Released
            },
        );
        pointer.proxy.frame();
        if down {
            self.buttons_down.insert((display, code));
        } else {
            self.buttons_down.remove(&(display, code));
            if self.held_buttons.contains(&(display, code)) {
                self.released_buttons.insert((display, code));
            }
        }
        Ok(())
    }

    fn release_buttons(&mut self) {
        for (display, code) in self.held_buttons.clone() {
            let _ = self.button_on(display, code, false, Instant::now());
        }
        self.stop_scroll();
    }

    fn stop_scroll(&mut self) {
        for pointer in self.pointers.values_mut() {
            if pointer.smooth_x || pointer.smooth_y {
                if pointer.smooth_x {
                    pointer
                        .proxy
                        .axis_stop(time_ms(), wl_pointer::Axis::HorizontalScroll);
                    pointer.proxy.axis_source(wl_pointer::AxisSource::Finger);
                }
                if pointer.smooth_y {
                    pointer
                        .proxy
                        .axis_stop(time_ms(), wl_pointer::Axis::VerticalScroll);
                    pointer.proxy.axis_source(wl_pointer::AxisSource::Finger);
                }
                pointer.proxy.frame();
                pointer.smooth_x = false;
                pointer.smooth_y = false;
            }
            pointer.remainder_x = 0;
            pointer.remainder_y = 0;
        }
    }

    /// Release everything held while injection is blocked (closed gate or paused refresh).
    fn blocked_release(&mut self) -> bool {
        if !self.blocked() {
            return false;
        }
        let needed = !self.xkb_down.is_empty()
            || !self.buttons_down.is_empty()
            || self
                .pointers
                .values()
                .any(|p| p.smooth_x || p.smooth_y || p.remainder_x != 0 || p.remainder_y != 0);
        if needed {
            self.release_keys();
            self.release_buttons();
        }
        needed
    }

    fn move_to(
        &mut self,
        display: DisplayId,
        position: PointDevice,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        let pointer = self.pointers.get(&display).ok_or(PlatformError::NotFound)?;
        let output = self
            .events
            .outputs
            .get(&pointer.output)
            .ok_or(PlatformError::NotFound)?;
        let (output_width, output_height) = output.extent();
        let width = output_width
            .checked_mul(256)
            .ok_or(PlatformError::Unsupported("output extent too large"))?;
        let height = output_height
            .checked_mul(256)
            .ok_or(PlatformError::Unsupported("output extent too large"))?;
        if width == 0
            || height == 0
            || !position.x.is_finite()
            || !position.y.is_finite()
            || position.x < 0.0
            || position.y < 0.0
            || position.x >= f64::from(output_width)
            || position.y >= f64::from(output_height)
        {
            return Err(PlatformError::Unsupported(
                "position outside output device pixels",
            ));
        }
        self.allowed(deadline)?;
        let x = (position.x * 256.0).round() as u32;
        let y = (position.y * 256.0).round() as u32;
        // Publish before submission so a monitor reading the resulting cursor cannot mistake
        // injected motion for local activity. Invalid, paused, or gated requests never get here.
        self.injected.record(
            display,
            PointDevice::new(f64::from(x) / 256.0, f64::from(y) / 256.0),
        );
        pointer
            .proxy
            .motion_absolute(time_ms(), x, y, width, height);
        pointer.proxy.frame();
        self.active = Some(display);
        Ok(())
    }

    fn scroll(&mut self, delta: ScrollDelta, deadline: Instant) -> Result<(), PlatformError> {
        if delta.pixels.is_some_and(|p| {
            !p.x.is_finite()
                || !p.y.is_finite()
                || [-p.x, -p.y].into_iter().any(|value| {
                    value < f64::from(i32::MIN) / 256.0 || value > f64::from(i32::MAX) / 256.0
                })
        }) {
            return Err(PlatformError::Unsupported(
                "scroll outside Wayland fixed-point range",
            ));
        }
        self.allowed(deadline)?;
        let pointer = self
            .pointers
            .get_mut(&self.active.ok_or(PlatformError::NotFound)?)
            .ok_or(PlatformError::NotFound)?;
        // Wayland's axis signs are opposite the shared HID/winit convention, independent of the
        // user's natural-scroll choice, which capture already applied.
        let ending = matches!(
            delta.phase,
            ScrollPhase::Ended | ScrollPhase::Cancelled | ScrollPhase::MomentumEnded
        );
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        let mut moved = false;
        let wheel =
            delta.phase == ScrollPhase::Discrete && (delta.v120_x != 0 || delta.v120_y != 0);
        if let Some(pixels) = delta.pixels.filter(|_| !wheel) {
            for (axis, value, active) in [
                (
                    wl_pointer::Axis::HorizontalScroll,
                    pixels.x,
                    &mut pointer.smooth_x,
                ),
                (
                    wl_pointer::Axis::VerticalScroll,
                    pixels.y,
                    &mut pointer.smooth_y,
                ),
            ] {
                if value != 0.0 {
                    if !self.gate.is_open() {
                        pointer.proxy.frame();
                        return Err(PlatformError::Locked);
                    }
                    pointer.proxy.axis(time_ms(), axis, -value);
                    // Hyprland associates source with the most recently selected axis, and an
                    // axis request resets that axis's source. Set it after each axis request.
                    pointer.proxy.axis_source(wl_pointer::AxisSource::Finger);
                    *active = true;
                    moved = true;
                }
            }
        } else {
            for (axis, value, remainder) in [
                (
                    wl_pointer::Axis::HorizontalScroll,
                    delta.v120_x,
                    &mut pointer.remainder_x,
                ),
                (
                    wl_pointer::Axis::VerticalScroll,
                    delta.v120_y,
                    &mut pointer.remainder_y,
                ),
            ] {
                if value != 0 {
                    let steps = scroll_steps(remainder, value);
                    if steps != 0 {
                        if !self.gate.is_open() {
                            pointer.proxy.frame();
                            return Err(PlatformError::Locked);
                        }
                        // This protocol's discrete argument is whole detents. Carry v120
                        // fractions until a detent is available; never add a second pixel delta.
                        pointer.proxy.axis_discrete(
                            time_ms(),
                            axis,
                            -steps as f64 * 10.0,
                            -steps as i32,
                        );
                        pointer.proxy.axis_source(wl_pointer::AxisSource::Wheel);
                        moved = true;
                    }
                }
            }
        }
        // Hyprland keeps one pending value per axis; separate a final displacement from its stop
        // so axis_stop cannot replace the displacement in that same pending frame.
        if moved && (delta.stop_x || delta.stop_y || ending) {
            pointer.proxy.frame();
        }
        for (axis, stop, active, remainder) in [
            (
                wl_pointer::Axis::HorizontalScroll,
                delta.stop_x || ending,
                &mut pointer.smooth_x,
                &mut pointer.remainder_x,
            ),
            (
                wl_pointer::Axis::VerticalScroll,
                delta.stop_y || ending,
                &mut pointer.smooth_y,
                &mut pointer.remainder_y,
            ),
        ] {
            if stop {
                pointer.proxy.axis_stop(time_ms(), axis);
                pointer.proxy.axis_source(wl_pointer::AxisSource::Finger);
                *active = false;
                *remainder = 0;
            }
        }
        pointer.proxy.frame();
        Ok(())
    }

    fn set_locks(
        &mut self,
        current: LockKeys,
        wanted: LockKeys,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        // ScrollLock is unavailable on this platform. Its wanted value is ignored, while
        // CapsLock and NumLock still apply. Unknown physical lock states remain unsupported.
        let keymap = self.xkb.get_keymap();
        let group = self.xkb.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
        let mut changes = Vec::new();
        for (symbol, now, want) in [
            (
                xkb::keysyms::KEY_Caps_Lock,
                current.caps_lock,
                wanted.caps_lock,
            ),
            (
                xkb::keysyms::KEY_Num_Lock,
                current.num_lock,
                wanted.num_lock,
            ),
        ] {
            if let Some(want) = want {
                let now = now.ok_or(PlatformError::Unsupported("lock key state unavailable"))?;
                if now != want {
                    changes.push(lock_keycode(&keymap, group, symbol));
                }
            }
        }
        let target = lock_mask(
            &keymap,
            self.xkb.serialize_mods(xkb::STATE_MODS_LOCKED),
            wanted,
        );
        if changes.is_empty() && target == self.xkb.serialize_mods(xkb::STATE_MODS_LOCKED) {
            return Ok(());
        }
        // Physical LEDs and this virtual source's locked state are independent on Hyprland.
        // Even when the physical value already matches, apply the requested mask to our source.
        self.allowed(deadline)?;
        self.xkb.update_mask(
            self.xkb.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            self.xkb.serialize_mods(xkb::STATE_MODS_LATCHED),
            lock_mask(
                &keymap,
                self.xkb.serialize_mods(xkb::STATE_MODS_LOCKED),
                current,
            ),
            0,
            0,
            group,
        );
        for code in changes.into_iter().flatten() {
            // The guard sends an up on any error after the tap starts. A caller-held key is
            // restored only if the gate still permits it; closed gates must leave it released.
            let held = self.held_keys.contains(&code);
            let mut tap = LockTap {
                source: self,
                code,
                complete: false,
            };
            if held {
                tap.source.key(code, false, deadline)?;
            }
            tap.source.key(code, true, deadline)?;
            tap.source.key(code, false, deadline)?;
            if held {
                tap.source.key(code, true, deadline)?;
            }
            tap.complete = true;
        }
        self.allowed(deadline)?;
        self.xkb.update_mask(
            self.xkb.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            self.xkb.serialize_mods(xkb::STATE_MODS_LATCHED),
            lock_mask(
                &keymap,
                self.xkb.serialize_mods(xkb::STATE_MODS_LOCKED),
                wanted,
            ),
            0,
            0,
            group,
        );
        self.modifiers();
        Ok(())
    }

    fn sync_request(&mut self) -> u64 {
        self.sync_serial += 1;
        self.connection
            .display()
            .sync(&self.queue.handle(), self.sync_serial);
        self.sync_serial
    }

    fn sync(&mut self, deadline: Instant) -> Result<(), PlatformError> {
        let mut target = self.sync_request();
        loop {
            if self.blocked_release() {
                // This barrier must follow the cleanup requests, not the earlier press.
                target = self.sync_request();
            }
            self.pump(deadline, TICK)?;
            if std::mem::take(&mut self.retired) {
                target = self.sync_request();
            }
            if self.events.synced >= target {
                for code in std::mem::take(&mut self.released_keys) {
                    self.held_keys.remove(&code);
                }
                for button in std::mem::take(&mut self.released_buttons) {
                    self.held_buttons.remove(&button);
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
        }
    }

    fn pump(&mut self, deadline: Instant, wait: Duration) -> Result<(), PlatformError> {
        self.queue
            .dispatch_pending(&mut self.events)
            .map_err(|_| backend("Wayland protocol or connection failure"))?;
        self.maintain_outputs();
        let writable = match self.connection.flush() {
            Ok(()) => false,
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => true,
            Err(_) => return Err(backend("Wayland protocol or connection failure")),
        };
        let Some(guard) = self.queue.prepare_read() else {
            return Ok(());
        };
        let fd = guard.connection_fd();
        let mut fds = [PollFd::new(
            &fd,
            if writable {
                PollFlags::IN | PollFlags::OUT
            } else {
                PollFlags::IN
            },
        )];
        let wait = wait.min(deadline.saturating_duration_since(Instant::now()));
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: i64::try_from(wait.as_nanos()).unwrap_or(0),
        };
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => return Ok(()),
            Err(_) => return Err(backend("could not poll injection connection")),
        }
        if fds[0]
            .revents()
            .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
        {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => return Err(backend("Wayland protocol or connection failure")),
            }
        }
        self.queue
            .dispatch_pending(&mut self.events)
            .map_err(|_| backend("Wayland protocol or connection failure"))?;
        self.maintain_outputs();
        Ok(())
    }

    fn close_keys(&mut self, deadline: Instant) {
        if self.keyboard.is_none() {
            return;
        }
        self.release_keys();
        if let Some(keyboard) = &self.keyboard {
            keyboard.modifiers(0, 0, 0, 0);
        }
        let _ = self.sync(deadline);
        if let Some(keyboard) = self.keyboard.take() {
            keyboard.destroy();
        }
        let _ = self.connection.flush();
    }

    fn close_pointers(&mut self, deadline: Instant) {
        self.pointers_open = false;
        if self.pointers.is_empty() {
            return;
        }
        self.release_buttons();
        let _ = self.sync(deadline);
        for (_, pointer) in std::mem::take(&mut self.pointers) {
            pointer.proxy.destroy();
        }
        self.active = None;
        let _ = self.connection.flush();
    }

    fn command(&mut self, action: Action, deadline: Instant) -> Result<(), PlatformError> {
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        if refused_while_paused(&self.refresh, self.gate.is_open(), &action) {
            return Err(PlatformError::Timeout);
        }
        let result = match action {
            Action::Key(usage, down) => self.key(keycode(usage)?, down, deadline),
            Action::ReleaseKeys => {
                self.release_keys();
                Ok(())
            }
            Action::RecoverKeys(keys) => {
                // Validate the entire journal before submitting any requests.
                let codes: Vec<_> = keys.into_iter().map(keycode).collect::<Result<_, _>>()?;
                for code in codes {
                    self.key(code, false, deadline)?;
                }
                Ok(())
            }
            Action::SetLocks(current, wanted) => self.set_locks(current, wanted, deadline),
            Action::Move(display, position) => self.move_to(display, position, deadline),
            Action::Button(button, down) => {
                let code = button_code(button)?;
                let owned: Vec<_> = self
                    .held_buttons
                    .iter()
                    .filter_map(|&(display, held)| (held == code).then_some(display))
                    .collect();
                let displays = if owned.is_empty() {
                    vec![self.active.ok_or(PlatformError::NotFound)?]
                } else {
                    owned
                };
                for display in displays {
                    self.button_on(display, code, down, deadline)?;
                }
                Ok(())
            }
            Action::Scroll(delta) => self.scroll(delta, deadline),
            Action::ReleaseButtons => {
                self.release_buttons();
                Ok(())
            }
            Action::RecoverButtons(buttons) => {
                let codes: Vec<_> = buttons
                    .into_iter()
                    .map(button_code)
                    .collect::<Result<_, _>>()?;
                let displays: Vec<_> = self.pointers.keys().copied().collect();
                if !codes.is_empty() && displays.is_empty() {
                    return Err(PlatformError::NotFound);
                }
                for code in codes {
                    for &display in &displays {
                        self.button_on(display, code, false, deadline)?;
                    }
                }
                Ok(())
            }
            Action::DropKeys => {
                self.close_keys(deadline);
                return Ok(());
            }
            Action::DropPointers => {
                self.close_pointers(deadline);
                return Ok(());
            }
        };
        // Even a partially submitted failed command must flush its releases and keep ownership
        // until a barrier confirms them. There are no key contents in backend errors or logs.
        let sync = self.sync(deadline);
        result?;
        sync?;
        Ok(())
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // One total deadline for both devices. Ups + cleared modifiers precede the barrier, and
        // device destruction follows it. A lost connection still leaves the engine's journal owed.
        let deadline = Instant::now() + CALL_BUDGET;
        self.release_keys();
        self.release_buttons();
        if let Some(keyboard) = &self.keyboard {
            keyboard.modifiers(0, 0, 0, 0);
        }
        let _ = self.sync(deadline);
        if let Some(keyboard) = self.keyboard.take() {
            keyboard.destroy();
        }
        for (_, pointer) in std::mem::take(&mut self.pointers) {
            pointer.proxy.destroy();
        }
        let _ = self.connection.flush();
    }
}

pub(super) fn run(
    mut source: Source,
    commands: Receiver<Command>,
    keys: Arc<AtomicBool>,
    pointers: Arc<AtomicBool>,
    watcher: Watcher,
) {
    work(
        &mut source,
        &commands,
        &keys,
        &pointers,
        &watcher.updates,
        &watcher.refresh,
    );
    // `source` drops here, after the watcher is no longer consulted: its drop releases whatever
    // is still held.
}

/// What the worker loop needs from the Wayland side, and the clock. The loop and the handling of
/// watcher answers (`work`, `apply_update`, `update_config`) are generic over it, so tests run
/// those very functions against a fake with a controlled clock, since a real `Source` needs a
/// compositor.
trait Worker {
    fn now(&self) -> Instant;
    fn shut_keys(&mut self, deadline: Instant);
    fn shut_pointers(&mut self, deadline: Instant);
    /// Release what is held while injection is blocked. False when the connection is gone.
    fn settle(&mut self, deadline: Instant) -> bool;
    /// An output changed since last asked, so a fresh configuration is wanted.
    fn take_output_change(&mut self) -> bool;
    fn refresh(&mut self) -> &mut Refresh;
    /// Check and install a configuration the watcher read. A rejected one changes nothing.
    fn apply_config(&mut self, config: Config, deadline: Instant) -> Result<(), PlatformError>;
    /// Whether nothing invalidated the configuration since a snapshot stamped `stamp` began.
    /// Called after `apply_config`, whose round trip delivers the outputs' latest events.
    fn is_current(&self, stamp: u64) -> bool;
    /// The worker just left the paused state: bind what was skipped while the map was unknown.
    fn resumed(&mut self);
    /// The Wayland connection itself is gone, as opposed to one request timing out.
    fn connection_lost(&self) -> bool;
    /// Record a failed refresh at `now`; true when that paused the worker.
    fn refresh_failed(&mut self, error: &PlatformError, now: Instant) -> bool;
    /// Wait for the next command, for one tick at most.
    fn wait_command(&mut self, commands: &Receiver<Command>) -> Result<Command, RecvTimeoutError>;
    /// Run one command and reply to it.
    fn serve(&mut self, command: Command);
    /// Housekeeping while no command arrived. False when the connection is gone.
    fn idle(&mut self, deadline: Instant) -> bool;
}

impl Worker for Source {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn shut_keys(&mut self, deadline: Instant) {
        self.close_keys(deadline);
    }

    fn shut_pointers(&mut self, deadline: Instant) {
        self.close_pointers(deadline);
    }

    fn settle(&mut self, deadline: Instant) -> bool {
        release_if_blocked(self, deadline)
    }

    fn take_output_change(&mut self) -> bool {
        std::mem::take(&mut self.refresh_outputs)
    }

    fn refresh(&mut self) -> &mut Refresh {
        &mut self.refresh
    }

    fn apply_config(&mut self, config: Config, deadline: Instant) -> Result<(), PlatformError> {
        self.install_config(config, deadline)
    }

    fn is_current(&self, stamp: u64) -> bool {
        snapshot_is_current(stamp, &self.config_epoch)
    }

    fn resumed(&mut self) {
        tracing::info!("injection configuration refreshed; input resumed");
        // Pointers skipped while the output map was unknown can be bound now.
        self.maintain_outputs();
    }

    fn connection_lost(&self) -> bool {
        self.connection.backend().last_error().is_some()
    }

    fn refresh_failed(&mut self, error: &PlatformError, now: Instant) -> bool {
        let entered = self.refresh.failed(now);
        if entered {
            // Error text only, never key contents.
            tracing::warn!(%error, "injection configuration refresh failed; input paused");
        } else {
            tracing::debug!(%error, "injection configuration refresh failed again");
        }
        entered
    }

    fn wait_command(&mut self, commands: &Receiver<Command>) -> Result<Command, RecvTimeoutError> {
        commands.recv_timeout(TICK)
    }

    fn serve(&mut self, command: Command) {
        let result = self.command(command.action, command.deadline);
        let _ = command.reply.send(result);
    }

    fn idle(&mut self, deadline: Instant) -> bool {
        self.pump(deadline, Duration::ZERO).is_ok()
    }
}

/// Whether a fresh configuration should be requested now: an output changed, or a paused
/// worker's retry is due.
fn wants_refresh(worker: &mut impl Worker) -> bool {
    let outputs = worker.take_output_change();
    let now = worker.now();
    // Both must run: a retry that comes due is consumed even when an output asked as well.
    let retry = worker.refresh().retry_due(now);
    outputs || retry
}

/// Handle a configuration the watcher read: the one place a refresh result becomes a state
/// change. A configuration that cannot be installed (for example its keymap does not compile)
/// is a failed refresh. While paused, success resumes the worker, but only if nothing
/// invalidated the configuration since the read began: otherwise the snapshot may predate an
/// output recreated under another id or a changed keyboard layout, so the worker stays paused
/// and the change's own refresh request supplies the next snapshot.
fn update_config(
    worker: &mut impl Worker,
    update: Update,
    deadline: Instant,
) -> Result<(), PlatformError> {
    let config = update.config?;
    worker.apply_config(config, deadline)?;
    if worker.refresh().is_paused() {
        if !worker.is_current(update.epoch) {
            return Err(backend("configuration changed while refreshing"));
        }
        worker.refresh().applied();
        worker.resumed();
    }
    Ok(())
}

/// Apply one watcher answer. False when the worker must end.
fn apply_update(worker: &mut impl Worker, update: Update) -> bool {
    // Each update gets its own budget; one slow update must not fail the next.
    let deadline = worker.now() + CALL_BUDGET;
    let Err(error) = update_config(worker, update, deadline) else {
        return true;
    };
    if worker.connection_lost() {
        return false;
    }
    // Fail closed: continuing with a stale layout or keymap is unsafe, but one failed refresh is
    // not the end of the worker. Release what is held, refuse input, and retry (see `Refresh`).
    let now = worker.now();
    if !worker.refresh_failed(&error, now) {
        return true;
    }
    let deadline = worker.now() + CALL_BUDGET;
    worker.settle(deadline)
}

fn work(
    worker: &mut impl Worker,
    commands: &Receiver<Command>,
    keys: &AtomicBool,
    pointers: &AtomicBool,
    updates: &Receiver<Update>,
    refresh: &SyncSender<()>,
) {
    loop {
        let deadline = worker.now() + CALL_BUDGET;
        if !keys.load(Ordering::Acquire) {
            worker.shut_keys(deadline);
        }
        if !pointers.load(Ordering::Acquire) {
            worker.shut_pointers(deadline);
        }
        if !keys.load(Ordering::Acquire) && !pointers.load(Ordering::Acquire) {
            return;
        }
        if !worker.settle(deadline) {
            return;
        }
        if wants_refresh(worker)
            && matches!(refresh.try_send(()), Err(TrySendError::Disconnected(_)))
        {
            // The watcher is gone, so no refresh can ever answer, running or paused. End the
            // worker, whose drop releases whatever is held, rather than inject with a
            // configuration nothing will ever update.
            return;
        }
        // At most one update per iteration: a watcher that keeps its one-slot channel full must
        // not keep the worker from commands (releases among them) and from the handle checks
        // above.
        match updates.try_recv() {
            Ok(update) => {
                if !apply_update(worker, update) {
                    return;
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return,
        }
        match worker.wait_command(commands) {
            Ok(command) => worker.serve(command),
            Err(RecvTimeoutError::Timeout) => {
                if !worker.idle(deadline) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Release what is held while injection is blocked and wait for the compositor to confirm it.
/// False when the Wayland connection is gone, which ends the worker.
fn release_if_blocked(source: &mut Source, deadline: Instant) -> bool {
    !(source.blocked_release() && source.sync(deadline).is_err() && source.connection_lost())
}

/// Whether a failed refresh refuses `action` with the retryable `Timeout`. A closed gate is not
/// handled here: it answers for itself (`Locked`) and wins, since both must be clear.
fn refused_while_paused(refresh: &Refresh, gate_open: bool, action: &Action) -> bool {
    refresh.is_paused() && gate_open && needs_known_config(action)
}

/// Whether `action` would inject input or depends on the output layout or keymap, so it must be
/// refused (retryably) while a failed refresh leaves those unknown. Everything else only releases
/// or recovers state the worker already owes, and is always honoured, so no key or button stays
/// down because of a pause.
fn needs_known_config(action: &Action) -> bool {
    match action {
        Action::Key(_, down) | Action::Button(_, down) => *down,
        Action::SetLocks(..) | Action::Move(..) | Action::Scroll(_) => true,
        Action::ReleaseKeys
        | Action::RecoverKeys(_)
        | Action::ReleaseButtons
        | Action::RecoverButtons(_)
        | Action::DropKeys
        | Action::DropPointers => false,
    }
}

struct LockTap<'a> {
    source: &'a mut Source,
    code: u16,
    complete: bool,
}

impl Drop for LockTap<'_> {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.source.key(self.code, false, Instant::now());
        }
    }
}

fn compile_keymap(names: &Rmlvo) -> Result<xkb::Keymap, PlatformError> {
    xkb::Keymap::new_from_names(
        &xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES),
        "evdev",
        "pc105",
        &names.layout,
        &names.variant,
        Some(names.options.clone()),
        xkb::COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| backend("could not compile target keyboard layout"))
}

/// Check a configuration the way connect does, changing nothing: the keymap it names must
/// compile. `None` when the names are unchanged, so the keymap in use stays.
fn validate_config(current: &Rmlvo, config: &Config) -> Result<Option<xkb::Keymap>, PlatformError> {
    if *current == config.names {
        return Ok(None);
    }
    compile_keymap(&config.names).map(Some)
}

/// The bound pointers, as `(display, name of its output)`, that `monitors` no longer maps to the
/// display they are keyed by.
fn stale_displays<'a>(
    bound: impl Iterator<Item = (DisplayId, Option<&'a str>)>,
    monitors: &BTreeMap<String, u32>,
) -> Vec<DisplayId> {
    bound
        .filter(|&(display, name)| {
            name.and_then(|name| monitors.get(name))
                .map(|&id| DisplayId(id))
                != Some(display)
        })
        .map(|(display, _)| display)
        .collect()
}

fn upload_keymap(
    keyboard: &ZwpVirtualKeyboardV1,
    keymap: &xkb::Keymap,
) -> Result<(), PlatformError> {
    let mut bytes = keymap
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        .into_bytes();
    bytes.push(0);
    let mut file = File::from(
        rustix::fs::memfd_create("crosspane-keymap", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|_| backend("could not create keymap memfd"))?,
    );
    file.write_all(&bytes)
        .map_err(|_| backend("could not write keymap memfd"))?;
    let size = u32::try_from(bytes.len()).map_err(|_| backend("keymap too large"))?;
    // One initial upload, then another only when the RMLVO tuple changes.
    keyboard.keymap(1, file.as_fd(), size);
    Ok(())
}

fn layout_group(keymap: &xkb::Keymap, config: &Config) -> u32 {
    config
        .layout_index
        .filter(|&i| i < keymap.num_layouts())
        .or_else(|| {
            (0..keymap.num_layouts())
                .find(|&i| config.active_keymap.as_deref() == Some(keymap.layout_get_name(i)))
        })
        .unwrap_or(0)
}

fn lock_keycode(keymap: &xkb::Keymap, group: u32, symbol: u32) -> Option<u16> {
    // Use the unshifted symbol in the active group; tapping a shifted-only binding would toggle
    // whatever unrelated symbol occupies its base level. In that case use modifiers alone.
    (keymap.min_keycode().raw()..=keymap.max_keycode().raw()).find_map(|code| {
        keymap
            .key_get_syms_by_level(code.into(), group, 0)
            .iter()
            .any(|s| s.raw() == symbol)
            .then(|| {
                code.checked_sub(8)
                    .and_then(|evdev| u16::try_from(evdev).ok())
            })
            .flatten()
    })
}

fn scroll_steps(remainder: &mut i64, value: i32) -> i64 {
    *remainder += i64::from(value);
    let steps = *remainder / 120;
    *remainder %= 120;
    steps
}

fn rotated_extent(width: u32, height: u32, transform: u32) -> (u32, u32) {
    if transform % 2 == 1 {
        (height, width)
    } else {
        (width, height)
    }
}

fn keycode(usage: HidUsage) -> Result<u16, PlatformError> {
    hid_to_evdev(usage).ok_or(PlatformError::Unsupported("unmapped HID usage"))
}

fn button_code(button: MouseButton) -> Result<u32, PlatformError> {
    match button.0 {
        1 => Ok(272),
        2 => Ok(273),
        3 => Ok(274),
        4 => Ok(275),
        5 => Ok(276),
        _ => Err(PlatformError::Unsupported("unmapped HID pointer button")),
    }
}

fn lock_mask(keymap: &xkb::Keymap, mut mask: u32, locks: LockKeys) -> u32 {
    for (name, wanted) in [
        (xkb::MOD_NAME_CAPS, locks.caps_lock),
        (xkb::MOD_NAME_NUM, locks.num_lock),
    ] {
        let index = keymap.mod_get_index(name);
        if let Some(wanted) = wanted
            && index < 32
        {
            let bit = 1 << index;
            if wanted {
                mask |= bit;
            } else {
                mask &= !bit;
            }
        }
    }
    mask
}

fn time_ms() -> u32 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    (now.tv_sec as u64)
        .wrapping_mul(1000)
        .wrapping_add(now.tv_nsec as u64 / 1_000_000) as u32
}

fn connect_display(deadline: Instant) -> Result<Connection, PlatformError> {
    let display =
        std::env::var_os("WAYLAND_DISPLAY").ok_or_else(|| backend("WAYLAND_DISPLAY is not set"))?;
    let mut path = PathBuf::from(display);
    if !path.is_absolute() {
        path = PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR")
                .ok_or_else(|| backend("XDG_RUNTIME_DIR is not set"))?,
        )
        .join(path);
    }
    let address = rustix::net::SocketAddrUnix::new(&path)
        .map_err(|_| backend("invalid Wayland socket path"))?;
    let fd = rustix::net::socket_with(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
        None,
    )
    .map_err(|_| backend("could not create Wayland socket"))?;
    loop {
        match rustix::net::connect(&fd, &address) {
            Ok(()) => break,
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
                if Instant::now() >= deadline {
                    return Err(PlatformError::Timeout);
                }
                std::thread::sleep(TICK);
            }
            Err(_) => return Err(backend("could not connect to Wayland compositor")),
        }
    }
    Connection::from_socket(UnixStream::from(fd))
        .map_err(|_| backend("could not initialize Wayland connection"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::config::note_event;
    use super::*;
    use crate::hyprland::ipc::IpcEvent;
    use std::sync::mpsc;
    use std::thread;

    fn keymap(options: &str) -> xkb::Keymap {
        compile_keymap(&Rmlvo {
            layout: "us".into(),
            variant: String::new(),
            options: options.into(),
        })
        .unwrap()
    }

    #[test]
    fn lock_masks_preserve_unrequested_modifiers() {
        let keymap = keymap("");
        let shift = 1 << keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
        let locks = LockKeys {
            caps_lock: Some(true),
            num_lock: Some(true),
            scroll_lock: Some(true),
        };
        let mask = lock_mask(&keymap, shift, locks);
        assert_ne!(mask & (1 << keymap.mod_get_index(xkb::MOD_NAME_CAPS)), 0);
        assert_ne!(mask & (1 << keymap.mod_get_index(xkb::MOD_NAME_NUM)), 0);
        assert_ne!(mask & shift, 0);
        assert_eq!(lock_mask(&keymap, mask, LockKeys::default()), mask);
        assert_eq!(
            lock_mask(
                &keymap,
                mask,
                LockKeys {
                    caps_lock: Some(false),
                    num_lock: Some(false),
                    scroll_lock: None
                }
            ),
            shift
        );
    }

    #[test]
    fn lock_keycodes_follow_the_keymap() {
        assert_eq!(
            lock_keycode(&keymap(""), 0, xkb::keysyms::KEY_Caps_Lock),
            Some(58)
        );
        let remapped = keymap("compose:caps");
        assert_eq!(
            lock_keycode(&remapped, 0, xkb::keysyms::KEY_Caps_Lock),
            None
        );
        assert_eq!(
            lock_keycode(&remapped, 0, xkb::keysyms::KEY_Num_Lock),
            Some(69)
        );
    }

    fn injecting_actions() -> Vec<Action> {
        let wheel = ScrollDelta {
            v120_x: 0,
            v120_y: 120,
            pixels: None,
            phase: ScrollPhase::Discrete,
            stop_x: false,
            stop_y: false,
        };
        vec![
            Action::Key(HidUsage::keyboard(4), true),
            Action::Button(MouseButton::PRIMARY, true),
            Action::Move(DisplayId(0), PointDevice::new(1.0, 1.0)),
            Action::Scroll(wheel),
            Action::SetLocks(LockKeys::default(), LockKeys::default()),
        ]
    }

    fn releasing_actions() -> Vec<Action> {
        let usage = HidUsage::keyboard(4);
        vec![
            Action::Key(usage, false),
            Action::Button(MouseButton::PRIMARY, false),
            Action::ReleaseKeys,
            Action::RecoverKeys(vec![usage]),
            Action::ReleaseButtons,
            Action::RecoverButtons(vec![MouseButton::PRIMARY]),
            Action::DropKeys,
            Action::DropPointers,
        ]
    }

    #[test]
    fn a_paused_worker_refuses_injection_but_never_a_release() {
        let mut refresh = Refresh::default();
        let now = Instant::now();
        for action in injecting_actions().iter().chain(&releasing_actions()) {
            assert!(
                !refused_while_paused(&refresh, true, action),
                "a running worker refuses nothing"
            );
        }
        assert!(
            refresh.failed(now),
            "entering the pause releases held input"
        );
        for action in injecting_actions() {
            assert!(refused_while_paused(&refresh, true, &action));
        }
        // Key-up, button-up, recovery and teardown are honoured, so nothing stays down.
        for action in releasing_actions() {
            assert!(!refused_while_paused(&refresh, true, &action));
        }
        // A closed gate wins: its own `Locked` answer applies, not the retryable one.
        for action in injecting_actions() {
            assert!(!refused_while_paused(&refresh, false, &action));
        }
        assert!(refresh.applied());
        for action in injecting_actions() {
            assert!(!refused_while_paused(&refresh, true, &action));
        }
    }

    fn rmlvo(layout: &str) -> Rmlvo {
        Rmlvo {
            layout: layout.into(),
            variant: String::new(),
            options: String::new(),
        }
    }

    fn config(layout: &str) -> Config {
        Config {
            names: rmlvo(layout),
            active_keymap: None,
            layout_index: None,
            locks: LockKeys::default(),
            monitors: Vec::new(),
            keyboard_addresses: BTreeSet::new(),
        }
    }

    #[test]
    fn a_pointer_keyed_by_an_old_monitor_id_is_stale() {
        let monitors: BTreeMap<String, u32> = [("A".to_owned(), 2), ("B".to_owned(), 7)].into();
        let bound = [
            // Output A was recreated with a new monitor id; its pointer kept the old key.
            (DisplayId(1), Some("A")),
            // Still the right id for its output.
            (DisplayId(7), Some("B")),
            // Output no longer in the map, or not named yet.
            (DisplayId(9), Some("gone")),
            (DisplayId(4), None),
            // Another output's id is no excuse.
            (DisplayId(7), Some("A")),
        ];
        assert_eq!(
            stale_displays(bound.into_iter(), &monitors),
            [DisplayId(1), DisplayId(9), DisplayId(4), DisplayId(7)]
        );
        assert!(stale_displays([(DisplayId(2), Some("A"))].into_iter(), &monitors).is_empty());
    }

    /// A stand-in for the Wayland side, with a virtual clock and a fake watcher. The loop and the
    /// refresh-result handling run against it are the production ones (`work`, `apply_update`,
    /// `update_config`); only what needs a compositor is faked.
    struct Fake {
        clock: Instant,
        refresh: Refresh,
        epoch: Arc<AtomicU64>,
        /// The keymap in use.
        names: Rmlvo,
        configs_applied: usize,
        resumed: usize,
        entered_pause: usize,
        failures_at: Vec<Instant>,
        served_after_configs: Vec<usize>,
        log: Vec<&'static str>,
        /// Report an output change on every iteration, like a worker that keeps asking.
        output_changes_forever: bool,
        /// Drop both handles once this many configurations were applied.
        drop_handles_after: usize,
        keys: Arc<AtomicBool>,
        pointers: Arc<AtomicBool>,
        watcher: Option<FakeWatcher>,
        requests_at: Vec<Instant>,
    }

    /// The far end of the refresh channels: it answers every request with a failed read.
    struct FakeWatcher {
        requests: Receiver<()>,
        answers: mpsc::Sender<Update>,
        /// Drop the handles after this many requests, to end the loop.
        stop_after: usize,
    }

    impl Fake {
        fn new(keys: &Arc<AtomicBool>, pointers: &Arc<AtomicBool>) -> Self {
            Self {
                clock: Instant::now(),
                refresh: Refresh::default(),
                epoch: Arc::new(AtomicU64::new(0)),
                names: rmlvo("us"),
                configs_applied: 0,
                resumed: 0,
                entered_pause: 0,
                failures_at: Vec::new(),
                served_after_configs: Vec::new(),
                log: Vec::new(),
                output_changes_forever: false,
                drop_handles_after: usize::MAX,
                keys: keys.clone(),
                pointers: pointers.clone(),
                watcher: None,
                requests_at: Vec::new(),
            }
        }

        fn drop_handles(&self) {
            self.keys.store(false, Ordering::Release);
            self.pointers.store(false, Ordering::Release);
        }

        fn answer_requests(&mut self) {
            let Some(watcher) = &self.watcher else {
                return;
            };
            if watcher.requests.try_recv().is_ok() {
                self.requests_at.push(self.clock);
                let _ = watcher.answers.send(Update {
                    epoch: self.epoch.load(Ordering::Acquire),
                    config: Err(PlatformError::Timeout),
                });
                if self.requests_at.len() == watcher.stop_after {
                    self.drop_handles();
                }
            }
        }
    }

    impl Worker for Fake {
        fn now(&self) -> Instant {
            self.clock
        }
        fn shut_keys(&mut self, _: Instant) {
            self.log.push("shut_keys");
        }
        fn shut_pointers(&mut self, _: Instant) {
            self.log.push("shut_pointers");
        }
        fn settle(&mut self, _: Instant) -> bool {
            true
        }
        fn take_output_change(&mut self) -> bool {
            self.output_changes_forever
        }
        fn refresh(&mut self) -> &mut Refresh {
            &mut self.refresh
        }
        fn apply_config(&mut self, config: Config, _: Instant) -> Result<(), PlatformError> {
            self.configs_applied += 1;
            if self.configs_applied == self.drop_handles_after {
                self.drop_handles();
            }
            // The production check, as `Source` does before installing anything.
            if validate_config(&self.names, &config)?.is_some() {
                self.names = config.names;
            }
            Ok(())
        }
        fn is_current(&self, stamp: u64) -> bool {
            snapshot_is_current(stamp, &self.epoch)
        }
        fn resumed(&mut self) {
            self.resumed += 1;
        }
        fn connection_lost(&self) -> bool {
            false
        }
        fn refresh_failed(&mut self, _: &PlatformError, now: Instant) -> bool {
            self.failures_at.push(now);
            let entered = self.refresh.failed(now);
            self.entered_pause += usize::from(entered);
            entered
        }
        fn wait_command(
            &mut self,
            commands: &Receiver<Command>,
        ) -> Result<Command, RecvTimeoutError> {
            match commands.try_recv() {
                Ok(command) => Ok(command),
                Err(TryRecvError::Empty) => {
                    // One tick passes with no command, in which the watcher answers.
                    self.answer_requests();
                    self.clock += TICK;
                    Err(RecvTimeoutError::Timeout)
                }
                Err(TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
            }
        }
        fn serve(&mut self, command: Command) {
            self.served_after_configs.push(self.configs_applied);
            let _ = command.reply.send(Ok(()));
        }
        fn idle(&mut self, _: Instant) -> bool {
            true
        }
    }

    fn update(config: Config) -> Update {
        Update {
            epoch: 0,
            config: Ok(config),
        }
    }

    fn handles() -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        (
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
        )
    }

    /// Run the loop on a thread, so a loop that fails to end fails the test instead of hanging it.
    fn ends_within_five_seconds(run: impl FnOnce() + Send + 'static) -> bool {
        let (done, finished) = mpsc::sync_channel(1);
        thread::spawn(move || {
            run();
            let _ = done.send(());
        });
        finished.recv_timeout(Duration::from_secs(5)).is_ok()
    }

    #[test]
    fn an_invalid_then_a_valid_configuration_pause_and_resume_through_the_production_handling() {
        let (keys, pointers) = handles();
        let mut worker = Fake::new(&keys, &pointers);
        let injection = Action::Key(HidUsage::keyboard(4), true);
        let stamp = |worker: &Fake| worker.epoch.load(Ordering::Acquire);
        assert!(!refused_while_paused(&worker.refresh, true, &injection));

        // The watcher delivers a configuration whose keymap does not compile. Nothing here picks
        // the transition: the handling does, so the worker pauses by itself and refuses input.
        let invalid = config("no-such-layout");
        let epoch = stamp(&worker);
        assert!(apply_update(
            &mut worker,
            Update {
                epoch,
                config: Ok(invalid)
            }
        ));
        assert!(worker.refresh.is_paused(), "an invalid keymap must pause");
        assert_eq!(worker.entered_pause, 1);
        assert!(refused_while_paused(&worker.refresh, true, &injection));
        assert_eq!(
            worker.names,
            rmlvo("us"),
            "a rejected keymap changed nothing"
        );

        // The retry delivers the same invalid configuration: still paused, still refusing.
        worker.clock += Duration::from_millis(100);
        let epoch = stamp(&worker);
        assert!(apply_update(
            &mut worker,
            Update {
                epoch,
                config: Ok(config("no-such-layout"))
            }
        ));
        assert!(
            worker.refresh.is_paused(),
            "an invalid keymap must not resume"
        );
        assert_eq!(worker.entered_pause, 1, "one pause, not a new one");
        assert!(refused_while_paused(&worker.refresh, true, &injection));
        assert_eq!(worker.resumed, 0);

        // A valid configuration is installed and resumes, again by itself.
        worker.clock += Duration::from_millis(200);
        let epoch = stamp(&worker);
        assert!(apply_update(
            &mut worker,
            Update {
                epoch,
                config: Ok(config("de"))
            }
        ));
        assert!(!worker.refresh.is_paused());
        assert_eq!(worker.resumed, 1);
        assert_eq!(worker.names, rmlvo("de"));
        assert!(!refused_while_paused(&worker.refresh, true, &injection));
    }

    #[test]
    fn a_keyboard_change_while_a_paused_refresh_is_in_flight_keeps_it_paused() {
        for event in ["activelayout", "configreloaded"] {
            let (keys, pointers) = handles();
            let mut worker = Fake::new(&keys, &pointers);
            let injection = Action::Key(HidUsage::keyboard(4), true);
            let dirty = AtomicBool::new(false);
            assert!(apply_update(&mut worker, update(config("no-such-layout"))));
            assert!(worker.refresh.is_paused());

            // A retry begins and reads the keyboard configuration as it is now (layout "us")...
            let stamp = worker.epoch.load(Ordering::Acquire);
            // ...then the keyboard layout changes before the retry finishes. The outputs did not
            // change, so only the watcher's event thread knows.
            note_event(
                &IpcEvent::Event {
                    name: event,
                    data: "",
                },
                &dirty,
                &worker.epoch,
            );
            assert!(dirty.load(Ordering::Acquire), "the watcher will read again");
            // The retry's snapshot arrives. It is valid, but it predates the change.
            assert!(apply_update(
                &mut worker,
                Update {
                    epoch: stamp,
                    config: Ok(config("us"))
                }
            ));
            assert!(
                worker.refresh.is_paused(),
                "a keyboard snapshot read before {event} resumed injection"
            );
            assert!(refused_while_paused(&worker.refresh, true, &injection));
            assert_eq!(worker.resumed, 0);

            // The read the change asked for began after it, and resumes.
            let stamp = worker.epoch.load(Ordering::Acquire);
            assert!(apply_update(
                &mut worker,
                Update {
                    epoch: stamp,
                    config: Ok(config("de"))
                }
            ));
            assert!(!worker.refresh.is_paused(), "{event}");
            assert_eq!(worker.names, rmlvo("de"));
            assert_eq!(worker.resumed, 1);
        }
    }

    #[test]
    fn an_unrelated_event_during_a_paused_refresh_does_not_stop_it_resuming() {
        let (keys, pointers) = handles();
        let mut worker = Fake::new(&keys, &pointers);
        let dirty = AtomicBool::new(false);
        assert!(apply_update(&mut worker, update(config("no-such-layout"))));
        let stamp = worker.epoch.load(Ordering::Acquire);
        note_event(
            &IpcEvent::Event {
                name: "workspace",
                data: "2",
            },
            &dirty,
            &worker.epoch,
        );
        assert!(apply_update(
            &mut worker,
            Update {
                epoch: stamp,
                config: Ok(config("us"))
            }
        ));
        assert!(!worker.refresh.is_paused());
    }

    #[test]
    fn a_paused_worker_retries_on_the_exact_backoff_schedule() {
        let (keys, pointers) = handles();
        let (answers, updates) = mpsc::channel::<Update>();
        let (refresh, requests) = mpsc::sync_channel(1);
        let (_commands_tx, commands) = mpsc::sync_channel(32);
        let mut worker = Fake::new(&keys, &pointers);
        worker.watcher = Some(FakeWatcher {
            requests,
            answers: answers.clone(),
            stop_after: 8,
        });
        // The first refresh fails; after that the watcher fails every retry.
        answers
            .send(Update {
                epoch: 0,
                config: Err(PlatformError::Timeout),
            })
            .unwrap();
        work(&mut worker, &commands, &keys, &pointers, &updates, &refresh);

        // The delay from each failure to the next request is the schedule, to the millisecond:
        // the loop runs on the fake's virtual clock, so no real timing is involved.
        let delays: Vec<u128> = worker
            .failures_at
            .iter()
            .zip(&worker.requests_at)
            .map(|(failed, asked)| asked.duration_since(*failed).as_millis())
            .collect();
        assert_eq!(delays, [100, 200, 400, 800, 1600, 2000, 2000, 2000]);
        assert_eq!(worker.entered_pause, 1, "the pause was entered once");
        assert_eq!(worker.log, ["shut_keys", "shut_pointers"]);
    }

    #[test]
    fn a_watcher_that_never_stops_answering_cannot_starve_commands_or_teardown() {
        let (handle_keys, handle_pointers) = handles();
        // A hostile watcher: an answer is always waiting, however many the worker takes.
        let (updates_tx, updates) = mpsc::channel::<Update>();
        for _ in 0..10_000 {
            updates_tx.send(update(config("us"))).unwrap();
        }
        let (refresh, _requests) = mpsc::sync_channel(1);
        let (commands_tx, commands) = mpsc::sync_channel(32);
        // A release is already queued when the loop starts.
        let (reply, answered) = mpsc::sync_channel(1);
        commands_tx
            .send(Command {
                action: Action::ReleaseKeys,
                deadline: Instant::now() + Duration::from_secs(10),
                reply,
            })
            .unwrap();
        let mut fake = Fake::new(&handle_keys, &handle_pointers);
        fake.drop_handles_after = 3;
        work(
            &mut fake,
            &commands,
            &handle_keys,
            &handle_pointers,
            &updates,
            &refresh,
        );

        assert!(
            matches!(answered.try_recv(), Ok(Ok(()))),
            "the queued release was served"
        );
        assert!(
            fake.served_after_configs[0] <= 1,
            "the release waited for {} updates",
            fake.served_after_configs[0]
        );
        // The dropped handles were noticed within an iteration of the third update.
        assert!(
            fake.configs_applied <= 4,
            "{} updates ran after the handles were dropped",
            fake.configs_applied
        );
        assert_eq!(fake.log, ["shut_keys", "shut_pointers"]);
        drop((commands_tx, updates_tx));
    }

    #[test]
    fn a_lost_watcher_ends_the_worker_running_or_paused() {
        // (the watcher's answers are gone, its requests are gone, a refresh is wanted)
        for (answers_gone, requests_gone, output_changes, what) in [
            (true, false, false, "running, answers gone"),
            (true, false, true, "paused, answers gone"),
            (
                false,
                true,
                true,
                "running, an output changed, requests gone",
            ),
            (false, true, true, "paused, retry due, requests gone"),
        ] {
            let (handle_keys, handle_pointers) = handles();
            let (updates_tx, updates) = mpsc::sync_channel::<Update>(1);
            let (refresh, requests) = mpsc::sync_channel(1);
            let (_commands_tx, commands) = mpsc::sync_channel(32);
            // Whichever end the watcher would hold stays alive and silent, or is dropped.
            let (silent, kept) = (
                (!answers_gone).then_some(updates_tx),
                (!requests_gone).then_some(requests),
            );
            let mut fake = Fake::new(&handle_keys, &handle_pointers);
            fake.output_changes_forever = output_changes;
            assert!(
                ends_within_five_seconds(move || work(
                    &mut fake,
                    &commands,
                    &handle_keys,
                    &handle_pointers,
                    &updates,
                    &refresh,
                )),
                "the worker kept running without a watcher: {what}"
            );
            drop((silent, kept));
        }
    }

    #[test]
    fn hid_button_codes() {
        for (button, code) in [(1, 272), (2, 273), (3, 274), (4, 275), (5, 276)] {
            assert_eq!(button_code(MouseButton(button)).unwrap(), code);
        }
        assert!(matches!(
            button_code(MouseButton(0)),
            Err(PlatformError::Unsupported(_))
        ));
        assert!(button_code(MouseButton(6)).is_err());
    }

    #[test]
    fn signed_scroll_remainders() {
        let mut remainder = 0;
        assert_eq!(scroll_steps(&mut remainder, 60), 0);
        assert_eq!(scroll_steps(&mut remainder, 60), 1);
        assert_eq!(remainder, 0);
        assert_eq!(scroll_steps(&mut remainder, -70), 0);
        assert_eq!(scroll_steps(&mut remainder, 30), 0);
        assert_eq!(scroll_steps(&mut remainder, -200), -2);
        assert_eq!(remainder, 0);
        assert_eq!(
            scroll_steps(&mut remainder, i32::MAX),
            i64::from(i32::MAX) / 120
        );
        assert!(remainder.abs() < 120);
    }

    #[test]
    fn rotation_swaps_device_pixel_extents() {
        for transform in [0, 2, 4, 6] {
            assert_eq!(rotated_extent(1280, 720, transform), (1280, 720));
        }
        for transform in [1, 3, 5, 7] {
            assert_eq!(rotated_extent(1280, 720, transform), (720, 1280));
        }
        let mut output = Output {
            width: 1280,
            height: 720,
            ..Output::default()
        };
        assert_eq!(output.extent(), (1280, 720));
        output.transform = 3;
        assert_eq!(output.extent(), (720, 1280));
    }
}
