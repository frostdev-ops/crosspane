//! Test driver: replay virtual pointer/keyboard input into the compositor in `$WAYLAND_DISPLAY`.
//! For nested-compositor end-to-end tests only (refuses to run without `CROSSPANE_NESTED_HYPR=1`).
//!
//! Commands on stdin, one per line:
//!   abs X Y        absolute pointer position on the first output (pixels)
//!   rel DX DY      relative pointer motion
//!   btn down|up    left button
//!   key CODE down|up   evdev key code
//!   sleep MS
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::time::Duration;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use xkbcommon::xkb;

#[derive(Default)]
struct State {
    size: Option<(u32, u32)>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Mode { width, height, .. } = event {
            state.size = Some((width as u32, height as u32));
        }
    }
}
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);

fn now_ms() -> u32 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    (t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000) as u32
}

fn main() {
    assert_eq!(std::env::var("CROSSPANE_NESTED_HYPR").as_deref(), Ok("1"), "nested compositors only");
    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut queue) = registry_queue_init::<State>(&conn).unwrap();
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).unwrap();
    let output: wl_output::WlOutput = globals.bind(&qh, 1..=4, ()).unwrap();
    let vpm: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = vpm.create_virtual_pointer_with_output(Some(&seat), Some(&output), &qh, ());
    let vkm: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let keyboard = vkm.create_virtual_keyboard(&seat, &qh, ());
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS).unwrap();
    let mut text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1).into_bytes();
    text.push(0);
    let mut file = std::fs::File::from(rustix::fs::memfd_create("vinput-keymap", rustix::fs::MemfdFlags::CLOEXEC).unwrap());
    file.write_all(&text).unwrap();
    keyboard.keymap(1, file.as_fd(), text.len() as u32);
    let mut state = State::default();
    queue.roundtrip(&mut state).unwrap();
    queue.roundtrip(&mut state).unwrap();
    let (w, h) = state.size.expect("output mode");
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["abs", x, y] => {
                pointer.motion_absolute(now_ms(), x.parse().unwrap(), y.parse().unwrap(), w, h);
                pointer.frame();
            }
            ["rel", dx, dy] => {
                pointer.motion(now_ms(), dx.parse().unwrap(), dy.parse().unwrap());
                pointer.frame();
            }
            ["btn", s] => {
                let state = if *s == "down" { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
                pointer.button(now_ms(), 0x110, state);
                pointer.frame();
            }
            ["key", code, s] => keyboard.key(now_ms(), code.parse().unwrap(), u32::from(*s == "down")),
            ["sleep", ms] => std::thread::sleep(Duration::from_millis(ms.parse().unwrap())),
            [] => {}
            other => eprintln!("unknown command {other:?}"),
        }
        queue.roundtrip(&mut state).unwrap();
    }
}
