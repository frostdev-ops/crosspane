//! Thread-confined virtual devices, conservative ownership and bounded Wayland synchronization.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
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

use super::config::{Config, Rmlvo, Watcher};
use super::{Action, CALL_BUDGET, Command, backend};

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
}

impl Source {
    pub fn new(
        gate: Arc<IoGate>,
        config: Config,
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
                if let Some(pointer) = self.pointers.remove(&display) {
                    // Retiring a device still owes explicit releases, even with the gate closed.
                    for &(held_display, code) in &self.held_buttons {
                        if held_display == display {
                            pointer.proxy.button(
                                time_ms(),
                                code,
                                wl_pointer::ButtonState::Released,
                            );
                            self.released_buttons.insert((display, code));
                            self.buttons_down.remove(&(display, code));
                        }
                    }
                    pointer.proxy.frame();
                    pointer.proxy.destroy();
                    self.retired = true;
                }
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
        if self.pointers_open
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
        }
    }

    fn update_config(&mut self, config: Config, deadline: Instant) -> Result<(), PlatformError> {
        self.monitors = config.monitors.iter().cloned().collect();
        self.maintain_outputs();
        if self.keyboard.is_none() {
            return Ok(());
        }
        if self.names != config.names {
            let keymap = compile_keymap(&config.names)?;
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
            if self.gate.is_open() {
                self.allowed(deadline)?;
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
                    self.allowed(deadline)?;
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
        Ok(())
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

    fn gate_release(&mut self) -> bool {
        if self.gate.is_open() {
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
        pointer.proxy.motion_absolute(
            time_ms(),
            (position.x * 256.0).round() as u32,
            (position.y * 256.0).round() as u32,
            width,
            height,
        );
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
            if self.gate_release() {
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
    loop {
        let deadline = Instant::now() + CALL_BUDGET;
        if !keys.load(Ordering::Acquire) {
            source.close_keys(deadline);
        }
        if !pointers.load(Ordering::Acquire) {
            source.close_pointers(deadline);
        }
        if !keys.load(Ordering::Acquire) && !pointers.load(Ordering::Acquire) {
            return;
        }
        if source.gate_release()
            && source.sync(deadline).is_err()
            && source.connection.backend().last_error().is_some()
        {
            return;
        }
        if std::mem::take(&mut source.refresh_outputs) {
            let _ = watcher.refresh.try_send(());
        }
        while let Ok(update) = watcher.updates.try_recv() {
            if update
                .and_then(|config| source.update_config(config, deadline))
                .is_err()
            {
                // Continuing with a stale layout after a failed refresh is unsafe.
                return;
            }
        }
        match commands.recv_timeout(TICK) {
            Ok(command) => {
                let result = source.command(command.action, command.deadline);
                let _ = command.reply.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {
                if source.pump(deadline, Duration::ZERO).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
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
    use super::*;

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
