//! WP-1.12/1.13 virtual input against a disposable nested Hyprland and the event fixture.
//! One test owns an isolated named nest, so default Nextest parallelism cannot steal its focus.
//! Raw destruction, our explicit Drop and recovery are checked sequentially in that nest.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crosspane_platform::{IoGate, KeyInjector, PlatformError, PointerInjector};
use crosspane_platform_linux::hyprland::inject::connect;
use crosspane_platform_linux::hyprland::ipc::HyprIpc;
use crosspane_types::geom::{PointDevice, VectorLogical};
use crosspane_types::hid::{HidUsage, MouseButton, hid_to_evdev};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use serde_json::Value;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_callback, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
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
struct Events {
    synced: bool,
    keymap: Option<xkb::Keymap>,
    keymaps: usize,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Events {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.synced = true;
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Events {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { fd, size, .. } = event {
            let mut text = String::new();
            File::from(fd)
                .take(u64::from(size))
                .read_to_string(&mut text)
                .unwrap();
            let text = text.trim_end_matches('\0');
            state.keymap = Some(
                xkb::Keymap::new_from_string(
                    &xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES),
                    text.to_owned(),
                    xkb::KEYMAP_FORMAT_TEXT_V1,
                    xkb::COMPILE_NO_FLAGS,
                )
                .unwrap(),
            );
            state.keymaps += 1;
        }
    }
}

delegate_noop!(Events: ignore wl_seat::WlSeat);
delegate_noop!(Events: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(Events: ignore ZwpVirtualKeyboardV1);
delegate_noop!(Events: ignore wl_output::WlOutput);
delegate_noop!(Events: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(Events: ignore ZwlrVirtualPointerV1);

fn sync(connection: &Connection, queue: &mut EventQueue<Events>, events: &mut Events) {
    events.synced = false;
    connection.display().sync(&queue.handle(), ());
    connection.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        queue.dispatch_pending(events).unwrap();
        if events.synced {
            return;
        }
        assert!(Instant::now() < deadline, "nested Wayland sync timed out");
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: 10_000_000,
        };
        let fd = guard.connection_fd();
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        if rustix::event::poll(&mut fds, Some(&timeout)).unwrap() > 0 {
            guard.read().unwrap();
        }
    }
}

struct Fixture {
    process: Child,
    directory: PathBuf,
    events: PathBuf,
}

impl Fixture {
    fn start(ipc: &HyprIpc) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let directory = std::env::temp_dir().join(format!(
            "crosspane-wp-1-12-inject-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let events = directory.join("events.jsonl");
        // Build the named fixture with `cargo build -p crosspane-testapp` before the nested run.
        let executable = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("crosspane-testapp");
        let log = File::create(directory.join("fixture.log")).unwrap();
        let process = Command::new(executable)
            .args(["window", "--title", "crosspane-wp-1-12-inject"])
            .arg("--events")
            .arg(&events)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        let mut fixture = Self {
            process,
            directory,
            events,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                fixture.process.try_wait().unwrap().is_none(),
                "fixture exited: {}",
                fs::read_to_string(fixture.directory.join("fixture.log")).unwrap()
            );
            if fixture.records().iter().any(|e| e["event"] == "ready") {
                let clients = ipc.json("clients").unwrap();
                if let Some(client) = clients
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["pid"].as_u64() == Some(u64::from(fixture.process.id())))
                {
                    let selector = format!("address:{}", client["address"].as_str().unwrap());
                    ipc.dispatch(&format!(
                        "hl.dsp.focus({{ window = {} }})",
                        serde_json::to_string(&selector).unwrap()
                    ))
                    .unwrap();
                    if ipc.json("activewindow").unwrap()["pid"].as_u64()
                        == Some(u64::from(fixture.process.id()))
                    {
                        return fixture;
                    }
                }
            }
            assert!(Instant::now() < deadline, "fixture never became focused");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn records(&self) -> Vec<Value> {
        fs::read_to_string(&self.events)
            .unwrap_or_default()
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn key_count(&self, state: &str) -> usize {
        self.records()
            .iter()
            .filter(|e| {
                e["event"] == "key"
                    && e["code"] == "F24"
                    && e["state"] == state
                    && e["repeat"] == false
            })
            .count()
    }

    fn wait(&self, predicate: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let records = self.records();
            if predicate(&records) {
                return records;
            }
            assert!(
                Instant::now() < deadline,
                "fixture events did not arrive: {:?}",
                &records[records.len().saturating_sub(10)..]
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only terminate the child this test spawned, after verifying the nested opt-in.
        let _ = self.process.kill();
        let _ = self.process.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn nested() -> Option<HyprIpc> {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: needs a nested Hyprland (scripts/hypr-nested.sh env --name wp-1-12)");
        return None;
    }
    assert!(
        std::env::var_os("WAYLAND_SOCKET").is_none(),
        "inherited WAYLAND_SOCKET could override the nested display"
    );
    Some(HyprIpc::from_env().unwrap())
}

fn open_gate() -> std::sync::Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}

fn raw_destruction_requires_explicit_drop_and_journal_recovery() {
    let Some(ipc) = nested() else { return };
    let version = ipc.require_supported().unwrap();
    eprintln!(
        "nested Hyprland: {}.{}.{}",
        version.major, version.minor, version.patch
    );
    let close_option = ipc
        .json("getoption input:virtualkeyboard:release_pressed_on_close")
        .unwrap();
    eprintln!("release_pressed_on_close: {close_option}");

    let connection = Connection::connect_to_env().unwrap();
    let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 4..=9, ()).unwrap();
    let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let output: wl_output::WlOutput = globals.bind(&qh, 4..=4, ()).unwrap();
    let pointer_manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 2..=2, ()).unwrap();
    let raw_pointer =
        pointer_manager.create_virtual_pointer_with_output(Some(&seat), Some(&output), &qh, ());
    // Keep a second keyboard on the seat, as on a desktop with a physical keyboard. Otherwise
    // removing the only keyboard can remove the seat capability and mask a missing release.
    let control_keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
    let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "evdev",
        "pc105",
        "us",
        "",
        Some(String::new()),
        xkb::COMPILE_NO_FLAGS,
    )
    .unwrap();
    let mut bytes = keymap
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        .into_bytes();
    bytes.push(0);
    let mut file = File::from(
        rustix::fs::memfd_create("crosspane-inject-test", rustix::fs::MemfdFlags::CLOEXEC).unwrap(),
    );
    file.write_all(&bytes).unwrap();
    // Exactly one upload per device; the control keyboard never sends a press.
    for device in [&control_keyboard, &keyboard] {
        device.keymap(1, file.as_fd(), u32::try_from(bytes.len()).unwrap());
    }
    let mut events = Events::default();
    sync(&connection, &mut queue, &mut events);
    drop(file);

    let fixture = Fixture::start(&ipc);
    let evdev = u32::from(hid_to_evdev(HidUsage::keyboard(0x73)).unwrap());
    keyboard.key(0, evdev, 1);
    sync(&connection, &mut queue, &mut events);
    let deadline = Instant::now() + Duration::from_secs(2);
    while fixture.key_count("pressed") == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fixture.key_count("pressed"),
        1,
        "fixture did not receive F24 down"
    );
    keyboard.destroy();
    sync(&connection, &mut queue, &mut events);
    let deadline = Instant::now() + Duration::from_secs(1);
    while fixture.key_count("released") == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let releases = fixture.key_count("released");
    let mut held = BTreeSet::new();
    for event in fixture
        .records()
        .into_iter()
        .filter(|e| e["event"] == "key")
    {
        if event["state"] == "pressed" {
            held.insert(event["code"].as_str().unwrap().to_owned());
        } else {
            held.remove(event["code"].as_str().unwrap());
        }
    }
    eprintln!(
        "after destroy: F24 downs=1, ups={releases}, held={:?}",
        held
    );
    // Platform fact, approved by the lead: raw destruction does not release on Hyprland 0.56.
    assert_eq!(close_option["bool"], false);
    assert_eq!(releases, 0);
    assert!(held.contains("F24"));
    let monitors = ipc.json("monitors").unwrap();
    let monitor = &monitors.as_array().unwrap()[0];
    let client = ipc.json("activewindow").unwrap();
    let scale = monitor["scale"].as_f64().unwrap();
    let x = (client["at"][0].as_f64().unwrap() - monitor["x"].as_f64().unwrap()
        + client["size"][0].as_f64().unwrap() / 2.0)
        * scale;
    let y = (client["at"][1].as_f64().unwrap() - monitor["y"].as_f64().unwrap()
        + client["size"][1].as_f64().unwrap() / 2.0)
        * scale;
    raw_pointer.motion_absolute(
        0,
        x as u32,
        y as u32,
        monitor["width"].as_u64().unwrap() as u32,
        monitor["height"].as_u64().unwrap() as u32,
    );
    raw_pointer.frame();
    sync(&connection, &mut queue, &mut events);
    fixture.wait(|r| r.iter().any(|e| e["event"] == "pointer"));
    raw_pointer.button(0, 272, wl_pointer::ButtonState::Pressed);
    raw_pointer.frame();
    sync(&connection, &mut queue, &mut events);
    fixture.wait(|r| {
        r.iter()
            .any(|e| e["event"] == "button" && e["button"] == 1 && e["state"] == "pressed")
    });
    raw_pointer.destroy();
    sync(&connection, &mut queue, &mut events);
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !fixture
            .records()
            .iter()
            .any(|e| e["event"] == "button" && e["state"] == "released"),
        "raw pointer destruction unexpectedly released the button"
    );
    let gate = open_gate();
    let (mut keys, mut pointers) = connect(gate.clone(), ipc.clone()).unwrap();
    gate.set_engine_permits(false);
    keys.recover_keys(&[HidUsage::keyboard(0x73)]).unwrap();
    pointers.recover_buttons(&[MouseButton::PRIMARY]).unwrap();
    fixture.wait(|r| {
        r.iter()
            .any(|e| e["event"] == "button" && e["button"] == 1 && e["state"] == "released")
    });
    eprintln!("fresh pointer recovery: button up received (gate closed)");
    let deadline = Instant::now() + Duration::from_secs(2);
    while fixture.key_count("released") == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!(
        "after recovery up: F24 ups={}",
        fixture.key_count("released")
    );
    assert_eq!(
        fixture.key_count("released"),
        1,
        "recovery up was not received"
    );
    gate.set_engine_permits(true);
    keys.key(HidUsage::keyboard(0x73), true).unwrap();
    fixture.wait(|_| fixture.key_count("pressed") == 2);
    gate.set_engine_permits(false);
    let start = Instant::now();
    drop(keys);
    assert!(start.elapsed() < Duration::from_millis(50));
    fixture.wait(|_| fixture.key_count("released") == 2);
    eprintln!("our Drop: F24 downs=2, ups=2 (gate closed)");
    drop(pointers);
    control_keyboard.destroy();
    sync(&connection, &mut queue, &mut events);
}

fn keys(records: &[Value]) -> Vec<&Value> {
    records
        .iter()
        .filter(|e| e["event"] == "key" && e["repeat"] == false)
        .collect()
}

fn assert_keys_up(records: &[Value]) {
    let mut held = BTreeSet::new();
    for event in keys(records) {
        let code = event["code"].as_str().unwrap();
        if event["state"] == "pressed" {
            held.insert(code);
        } else {
            held.remove(code);
        }
    }
    assert!(held.is_empty(), "fixture has held keys: {held:?}");
}

struct Layout<'a> {
    ipc: &'a HyprIpc,
    previous: [String; 3],
}

impl<'a> Layout<'a> {
    fn set(ipc: &'a HyprIpc, layout: &str) -> Self {
        Self::with_options(ipc, layout, "")
    }

    fn with_options(ipc: &'a HyprIpc, layout: &str, options: &str) -> Self {
        let previous = ["input:kb_layout", "input:kb_variant", "input:kb_options"].map(|name| {
            ipc.json(&format!("getoption {name}")).unwrap()["str"]
                .as_str()
                .unwrap()
                .to_owned()
        });
        ipc.eval(&format!(
            "hl.config({{ input = {{ kb_layout = {}, kb_variant = \"\", kb_options = {} }} }})",
            serde_json::to_string(layout).unwrap(),
            serde_json::to_string(options).unwrap()
        ))
        .unwrap();
        Self { ipc, previous }
    }
}

impl Drop for Layout<'_> {
    fn drop(&mut self) {
        let [layout, variant, options] = self
            .previous
            .each_ref()
            .map(|s| serde_json::to_string(s).unwrap());
        self.ipc.eval(&format!("hl.config({{ input = {{ kb_layout = {layout}, kb_variant = {variant}, kb_options = {options} }} }})")).unwrap();
    }
}

fn wheel(x: i32, y: i32) -> ScrollDelta {
    ScrollDelta {
        v120_x: x,
        v120_y: y,
        pixels: None,
        phase: ScrollPhase::Discrete,
        stop_x: false,
        stop_y: false,
    }
}

fn fd_count() -> usize {
    fs::read_dir("/proc/self/fd").unwrap().count()
}

fn compositor_fd_count() -> usize {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap();
    // Find only the nested helper's matching state, rather than assuming a particular nest name.
    let state = fs::read_dir(runtime)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("crosspane-hypr-")
                && fs::read_to_string(path.join("env"))
                    .unwrap_or_default()
                    .lines()
                    .any(|line| line == format!("export HYPRLAND_INSTANCE_SIGNATURE={signature}"))
        })
        .unwrap();
    let pid: u32 = fs::read_to_string(state.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    fs::read_dir(format!("/proc/{pid}/fd")).unwrap().count()
}

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn physical_locks(ipc: &HyprIpc) -> LockKeys {
    let devices = ipc.json("devices").unwrap();
    let keyboards = devices["keyboards"].as_array().unwrap();
    let candidates: Vec<_> = keyboards
        .iter()
        .filter(|k| {
            !k["name"]
                .as_str()
                .unwrap()
                .starts_with("hl-virtual-keyboard")
        })
        .collect();
    let keyboard = candidates
        .iter()
        .find(|k| k["main"] == true)
        .copied()
        .or_else(|| candidates.first().copied())
        .unwrap();
    LockKeys {
        caps_lock: keyboard["capsLock"].as_bool(),
        num_lock: keyboard["numLock"].as_bool(),
        scroll_lock: None,
    }
}

fn virtual_locks(ipc: &HyprIpc) -> LockKeys {
    let devices = ipc.json("devices").unwrap();
    let keyboard = devices["keyboards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["main"] == true)
        .unwrap();
    assert!(
        keyboard["name"]
            .as_str()
            .unwrap()
            .starts_with("hl-virtual-keyboard")
    );
    LockKeys {
        caps_lock: keyboard["capsLock"].as_bool(),
        num_lock: keyboard["numLock"].as_bool(),
        scroll_lock: None,
    }
}

struct PrivateNest {
    name: String,
    config: PathBuf,
}
impl Drop for PrivateNest {
    fn drop(&mut self) {
        let output = Command::new(repository().join("scripts/hypr-nested.sh"))
            .args(["stop", "--name", &self.name])
            .output()
            .unwrap();
        fs::remove_file(&self.config).unwrap();
        assert!(output.status.success(), "private nested cleanup failed");
    }
}

#[test]
fn hyprland_injection_contract() {
    let Some(ipc) = nested() else { return };
    if std::env::var_os("CROSSPANE_INJECT_PRIVATE_NEST").is_none() {
        // Other crate tests open windows too. A file lock confined to this test binary cannot
        // serialize those read-only tests, so this test uses its own compositor and process env.
        let name = format!("wp-1-12-inject-{}", std::process::id());
        let config =
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join(format!("{name}.lua"));
        fs::write(
            &config,
            include_str!("../../../scripts/hypr-nested/hyprland.lua"),
        )
        .unwrap();
        let _nest = PrivateNest {
            name: name.clone(),
            config: config.clone(),
        };
        let mut start = Command::new(repository().join("scripts/hypr-nested.sh"));
        start
            .args(["start", "--name", &name, "--config"])
            .arg(&config);
        if std::env::var_os("CROSSPANE_PARENT_WAYLAND_DISPLAY").is_some() {
            start.env_remove("WAYLAND_DISPLAY");
        }
        let output = start.output().unwrap();
        assert!(
            output.status.success(),
            "nested start: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = Command::new("bash").current_dir(repository()).args(["-c",
            "eval \"$(scripts/hypr-nested.sh env --name \"$1\")\"; exec \"$2\" --exact hyprland_injection_contract --nocapture",
            "inject-nest", &name]).arg(std::env::current_exe().unwrap())
            .env("CROSSPANE_INJECT_PRIVATE_NEST", "1").env("CROSSPANE_INJECT_CONFIG", &config).output().unwrap();
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "isolated nested injection test failed"
        );
        return;
    }
    raw_destruction_requires_explicit_drop_and_journal_recovery();
    for (layout, expected) in [("us", "Hello, W;rld"), ("de", "Hello, Wörld")] {
        let _layout = Layout::set(&ipc, layout);
        let gate = open_gate();
        let (mut keyboard, pointer) = connect(gate, ipc.clone()).unwrap();
        let fixture = Fixture::start(&ipc);
        for (usage, shifted) in [
            (0x0b, true),
            (0x08, false),
            (0x0f, false),
            (0x0f, false),
            (0x12, false),
            (0x36, false),
            (0x2c, false),
            (0x1a, true),
            (0x33, false),
            (0x15, false),
            (0x0f, false),
            (0x07, false),
        ] {
            if shifted {
                keyboard.key(HidUsage::keyboard(0xe1), true).unwrap();
            }
            keyboard.key(HidUsage::keyboard(usage), true).unwrap();
            keyboard.key(HidUsage::keyboard(usage), false).unwrap();
            if shifted {
                keyboard.key(HidUsage::keyboard(0xe1), false).unwrap();
            }
        }
        let records = fixture.wait(|records| keys(records).len() >= 28);
        let text: String = keys(&records)
            .iter()
            .filter(|e| e["state"] == "pressed")
            .filter_map(|e| e["text"].as_str())
            .collect();
        assert_eq!(text, expected);
        assert_keys_up(&records);
        eprintln!("{layout}: expected/received {text:?}");
        drop(keyboard);
        drop(pointer);
    }

    let _layout = Layout::set(&ipc, "us");
    let gate = open_gate();
    let (mut keyboard, mut pointer) = connect(gate.clone(), ipc.clone()).unwrap();
    let fixture = Fixture::start(&ipc);
    let monitors = ipc.json("monitors").unwrap();
    let monitor = &monitors.as_array().unwrap()[0];
    let display = DisplayId(u32::try_from(monitor["id"].as_u64().unwrap()).unwrap());
    let width = monitor["width"].as_f64().unwrap();
    let height = monitor["height"].as_f64().unwrap();

    // A closed gate cannot type, move, press buttons or scroll; ups and recovery remain allowed.
    gate.set_engine_permits(false);
    assert!(matches!(
        keyboard.key(HidUsage::keyboard(4), true),
        Err(PlatformError::Locked)
    ));
    assert!(matches!(
        pointer.move_to(display, PointDevice::new(width / 2.0, height / 2.0)),
        Err(PlatformError::Locked)
    ));
    assert!(matches!(
        pointer.button(MouseButton::PRIMARY, true),
        Err(PlatformError::Locked)
    ));
    assert!(matches!(
        pointer.scroll(wheel(0, 120)),
        Err(PlatformError::Locked)
    ));
    keyboard.key(HidUsage::keyboard(4), false).unwrap();
    keyboard.recover_keys(&[HidUsage::keyboard(4)]).unwrap();
    pointer.button(MouseButton::PRIMARY, false).unwrap();
    pointer.recover_buttons(&[MouseButton::PRIMARY]).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert!(keys(&fixture.records()).is_empty());
    assert!(matches!(
        keyboard.key(
            HidUsage {
                page: 0xffff,
                id: 0xffff
            },
            true
        ),
        Err(PlatformError::Unsupported(_))
    ));
    assert!(matches!(
        pointer.button(MouseButton(6), true),
        Err(PlatformError::Unsupported(_))
    ));
    gate.set_engine_permits(true);
    eprintln!("closed gate: presses/motion/scroll refused; releases and recovery accepted");

    // Native repeat, including cancellation while no caller is making injection calls.
    keyboard.key(HidUsage::keyboard(4), true).unwrap();
    fixture.wait(|r| r.iter().any(|e| e["event"] == "key" && e["repeat"] == true));
    gate.set_session_permits(false);
    fixture.wait(|r| {
        r.iter()
            .any(|e| e["event"] == "key" && e["code"] == "KeyA" && e["state"] == "released")
    });
    std::thread::sleep(Duration::from_millis(100));
    let after_release = fixture.records().len();
    std::thread::sleep(Duration::from_millis(550));
    assert_eq!(
        fixture.records().len(),
        after_release,
        "repeat continued after gate closure"
    );
    keyboard.key(HidUsage::keyboard(4), false).unwrap();
    gate.set_session_permits(true);
    eprintln!("native repeat observed; idle gate closure stops it");

    let offset = fixture.records().len();
    let mut rng = 0x5036_b123_4567_89ab_u64;
    let candidates = [0xe1, 0xe5, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17];
    let mut expected_events = 0;
    for _ in 0..1000 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        for (bit, usage) in candidates.iter().enumerate() {
            if rng & (1 << bit) != 0 {
                keyboard.key(HidUsage::keyboard(*usage), true).unwrap();
                expected_events += 2;
            }
        }
        keyboard.release_all().unwrap();
        keyboard.release_all().unwrap();
    }
    let records = fixture.wait(|r| keys(&r[offset..]).len() >= expected_events);
    assert_eq!(keys(&records[offset..]).len(), expected_events);
    assert_keys_up(&records[offset..]);
    eprintln!("release_all: 1,000 seeded subsets; all app key transitions balanced");

    let offset = fixture.records().len();
    keyboard.key(HidUsage::keyboard(0x73), true).unwrap();
    keyboard.recover_keys(&[HidUsage::keyboard(0x73)]).unwrap();
    let records = fixture.wait(|r| keys(&r[offset..]).len() >= 2);
    assert_keys_up(&records[offset..]);

    let initial = keyboard.lock_keys().unwrap();
    assert_eq!(initial.scroll_lock, None);
    let wanted = LockKeys {
        caps_lock: Some(!initial.caps_lock.unwrap()),
        num_lock: Some(!initial.num_lock.unwrap()),
        scroll_lock: None,
    };
    keyboard.set_lock_keys(wanted).unwrap();
    assert_eq!(virtual_locks(&ipc), wanted);
    assert_eq!(keyboard.lock_keys().unwrap(), physical_locks(&ipc));
    keyboard.set_lock_keys(initial).unwrap();
    assert_eq!(keyboard.lock_keys().unwrap(), initial);
    keyboard.key(HidUsage::keyboard(0x39), true).unwrap();
    let held_lock = virtual_locks(&ipc);
    keyboard
        .set_lock_keys(LockKeys {
            caps_lock: Some(!held_lock.caps_lock.unwrap()),
            ..LockKeys::default()
        })
        .unwrap();
    assert_eq!(
        virtual_locks(&ipc).caps_lock,
        Some(!held_lock.caps_lock.unwrap())
    );
    keyboard.release_all().unwrap();
    keyboard.set_lock_keys(initial).unwrap();
    let offset = fixture.records().len();
    keyboard.key(HidUsage::keyboard(0xe1), true).unwrap();
    keyboard.key(HidUsage::keyboard(0xe5), true).unwrap();
    keyboard.set_lock_keys(wanted).unwrap();
    keyboard.set_lock_keys(initial).unwrap();
    keyboard.key(HidUsage::keyboard(0xe1), false).unwrap();
    keyboard.key(HidUsage::keyboard(4), true).unwrap();
    keyboard.key(HidUsage::keyboard(4), false).unwrap();
    keyboard.key(HidUsage::keyboard(0xe5), false).unwrap();
    keyboard.key(HidUsage::keyboard(4), true).unwrap();
    keyboard.key(HidUsage::keyboard(4), false).unwrap();
    let records = fixture.wait(|r| {
        keys(&r[offset..])
            .iter()
            .filter(|e| e["code"] == "KeyA")
            .count()
            >= 4
    });
    let text: String = keys(&records[offset..])
        .iter()
        .filter(|e| e["code"] == "KeyA" && e["state"] == "pressed")
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert_eq!(
        text,
        if initial.caps_lock == Some(true) {
            "aA"
        } else {
            "Aa"
        }
    );
    assert_keys_up(&records[offset..]);
    eprintln!(
        "CapsLock/NumLock virtual state round trip; held lock key preserved; physical IPC reads remain independent"
    );

    let client = ipc.json("activewindow").unwrap();
    assert_eq!(
        client["pid"].as_u64(),
        Some(u64::from(fixture.process.id()))
    );
    let scale = monitor["scale"].as_f64().unwrap();
    let ox = (client["at"][0].as_f64().unwrap() - monitor["x"].as_f64().unwrap()) * scale;
    let oy = (client["at"][1].as_f64().unwrap() - monitor["y"].as_f64().unwrap()) * scale;
    let sw = client["size"][0].as_f64().unwrap() * scale;
    let sh = client["size"][1].as_f64().unwrap() * scale;
    for (x, y) in [
        (sw / 4.0, sh / 4.0),
        (sw * 3.0 / 4.0, sh / 2.0),
        (sw / 2.0, sh * 3.0 / 4.0),
    ] {
        let offset = fixture.records().len();
        pointer
            .move_to(display, PointDevice::new(ox + x, oy + y))
            .unwrap();
        let records = fixture.wait(|r| r[offset..].iter().any(|e| e["event"] == "pointer"));
        let event = records[offset..]
            .iter()
            .rev()
            .find(|e| e["event"] == "pointer")
            .unwrap();
        assert!((event["x_px"].as_f64().unwrap() - x).abs() <= 1.0);
        assert!((event["y_px"].as_f64().unwrap() - y).abs() <= 1.0);
    }
    let offset = fixture.records().len();
    for button in 1..=5 {
        pointer.button(MouseButton(button), true).unwrap();
        pointer.button(MouseButton(button), false).unwrap();
    }
    let records = fixture.wait(|r| {
        r[offset..]
            .iter()
            .filter(|e| e["event"] == "button")
            .count()
            >= 10
    });
    for button in 1..=5 {
        for state in ["pressed", "released"] {
            assert!(
                records[offset..].iter().any(|e| e["event"] == "button"
                    && e["button"] == button
                    && e["state"] == state)
            );
        }
    }
    for (x, y) in [(0, 120), (0, -120), (120, 0), (-120, 0)] {
        let offset = fixture.records().len();
        pointer.scroll(wheel(x, y)).unwrap();
        let records = fixture.wait(|r| r[offset..].iter().any(|e| e["event"] == "wheel"));
        let event = records[offset..]
            .iter()
            .find(|e| e["event"] == "wheel")
            .unwrap();
        assert_eq!(event["lines_x"].as_f64().unwrap(), f64::from(x) / 120.0);
        assert_eq!(event["lines_y"].as_f64().unwrap(), f64::from(y) / 120.0);
    }
    let offset = fixture.records().len();
    pointer.scroll(wheel(0, 60)).unwrap();
    pointer.scroll(wheel(0, 60)).unwrap();
    let records = fixture.wait(|r| r[offset..].iter().any(|e| e["event"] == "wheel"));
    let lines: f64 = records[offset..]
        .iter()
        .filter(|e| e["event"] == "wheel")
        .map(|e| e["lines_y"].as_f64().unwrap())
        .sum();
    assert_eq!(lines, 1.0, "sub-detent v120 was counted twice");
    // Discrete wheels retain wheel semantics even when capture also supplies pixel values.
    let offset = fixture.records().len();
    pointer
        .scroll(ScrollDelta {
            pixels: Some(VectorLogical::new(7.0, 11.0)),
            ..wheel(120, 120)
        })
        .unwrap();
    fixture.wait(|r| {
        let lines = |axis: &str| {
            r[offset..]
                .iter()
                .filter(|e| e["event"] == "wheel")
                .filter_map(|e| e[axis].as_f64())
                .sum::<f64>()
        };
        lines("lines_x") == 1.0 && lines("lines_y") == 1.0
    });
    // For a smooth gesture, both representations describe one displacement: pixels win.
    let offset = fixture.records().len();
    pointer
        .scroll(ScrollDelta {
            pixels: Some(VectorLogical::new(7.0, 11.0)),
            phase: ScrollPhase::Began,
            ..wheel(120, 120)
        })
        .unwrap();
    let records = fixture.wait(|r| r[offset..].iter().any(|e| e["event"] == "wheel"));
    let event = records[offset..]
        .iter()
        .find(|e| e["event"] == "wheel")
        .unwrap();
    assert_eq!(event["px_x"].as_f64().unwrap(), 7.0 * scale);
    assert_eq!(event["px_y"].as_f64().unwrap(), 11.0 * scale);
    pointer
        .scroll(ScrollDelta {
            pixels: Some(VectorLogical::new(3.0, 5.0)),
            phase: ScrollPhase::Ended,
            stop_x: true,
            stop_y: true,
            ..wheel(0, 0)
        })
        .unwrap();
    fixture.wait(|r| {
        r[offset..]
            .iter()
            .filter(|e| {
                e["event"] == "wheel"
                    && e["px_x"].as_f64() == Some(3.0 * scale)
                    && e["px_y"].as_f64() == Some(5.0 * scale)
            })
            .count()
            == 1
    });
    pointer.release_all().unwrap();
    pointer.button(MouseButton::PRIMARY, true).unwrap();
    let offset = fixture.records().len();
    pointer.recover_buttons(&[MouseButton::PRIMARY]).unwrap();
    fixture.wait(|r| {
        r[offset..]
            .iter()
            .any(|e| e["event"] == "button" && e["state"] == "released")
    });
    eprintln!("pointer: 3 positions, buttons 1–5, signed wheel, smooth pixels/stops and recovery");

    for _ in 0..100 {
        keyboard.key(HidUsage::keyboard(0x73), true).unwrap();
        keyboard.key(HidUsage::keyboard(0x73), false).unwrap();
    }
    std::thread::sleep(Duration::from_millis(100));
    let offset = fixture.records().len();
    let before = fd_count();
    let compositor_before = compositor_fd_count();
    for _ in 0..10_000 {
        keyboard.key(HidUsage::keyboard(0x73), true).unwrap();
        keyboard.key(HidUsage::keyboard(0x73), false).unwrap();
    }
    let after = fd_count();
    assert_eq!(after, before);
    let records = fixture.wait(|r| keys(&r[offset..]).len() >= 20_000);
    assert_eq!(keys(&records[offset..]).len(), 20_000);
    assert_keys_up(&records[offset..]);
    std::thread::sleep(Duration::from_millis(100));
    let compositor_after = compositor_fd_count();
    assert_eq!(compositor_after, compositor_before);
    eprintln!(
        "10,000 key cycles: fds {before}→{after}, compositor fds {compositor_before}→{compositor_after}; all 20,000 app transitions received"
    );

    let offset = fixture.records().len();
    pointer.button(MouseButton::PRIMARY, true).unwrap();
    gate.set_engine_permits(false);
    let start = Instant::now();
    drop(pointer);
    assert!(start.elapsed() < Duration::from_millis(50));
    fixture.wait(|r| {
        r[offset..]
            .iter()
            .any(|e| e["event"] == "button" && e["state"] == "released")
    });
    drop(keyboard);
    runtime_layout_and_outputs(&ipc);
}

fn reload_layout(ipc: &HyprIpc, layout: &str, options: &str) {
    let config = PathBuf::from(std::env::var_os("CROSSPANE_INJECT_CONFIG").unwrap());
    fs::write(
        config,
        format!(
            "{}\nhl.config({{ input = {{ kb_layout = {}, kb_options = {} }} }})\n",
            include_str!("../../../scripts/hypr-nested/hyprland.lua"),
            serde_json::to_string(layout).unwrap(),
            serde_json::to_string(options).unwrap()
        ),
    )
    .unwrap();
    assert_eq!(ipc.request("reload").unwrap().trim(), "ok");
}

fn await_keymap(
    connection: &Connection,
    queue: &mut EventQueue<Events>,
    events: &mut Events,
    predicate: impl Fn(&xkb::Keymap) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        sync(connection, queue, events);
        if events.keymap.as_ref().is_some_and(&predicate) {
            return;
        }
        assert!(Instant::now() < deadline, "runtime keymap did not refresh");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn runtime_layout_and_outputs(ipc: &HyprIpc) {
    reload_layout(ipc, "us", "");
    let connection = Connection::connect_to_env().unwrap();
    let (globals, mut queue) = registry_queue_init::<Events>(&connection).unwrap();
    let seat: wl_seat::WlSeat = globals.bind(&queue.handle(), 4..=9, ()).unwrap();
    let _observer = seat.get_keyboard(&queue.handle(), ());
    let mut events = Events::default();
    let gate = open_gate();
    let (mut keyboard, mut pointer) = connect(gate, ipc.clone()).unwrap();
    let fixture = Fixture::start(ipc);
    await_keymap(&connection, &mut queue, &mut events, |k| {
        k.layout_get_name(0).contains("English")
    });
    let initial_maps = events.keymaps;
    for _ in 0..10 {
        keyboard.key(HidUsage::keyboard(0x73), true).unwrap();
        keyboard.key(HidUsage::keyboard(0x73), false).unwrap();
    }
    sync(&connection, &mut queue, &mut events);
    assert_eq!(events.keymaps, initial_maps, "keymap was uploaded per key");
    reload_layout(ipc, "de", "");
    await_keymap(&connection, &mut queue, &mut events, |k| {
        k.layout_get_name(0).contains("German")
    });
    let offset = fixture.records().len();
    keyboard.key(HidUsage::keyboard(0x33), true).unwrap();
    keyboard.key(HidUsage::keyboard(0x33), false).unwrap();
    let records = fixture.wait(|r| keys(&r[offset..]).len() >= 2);
    assert_eq!(keys(&records[offset..])[0]["text"], "ö");

    reload_layout(ipc, "us,de", "");
    await_keymap(&connection, &mut queue, &mut events, |k| {
        k.num_layouts() == 2
    });
    let devices = ipc.json("devices").unwrap();
    let physical_name = devices["keyboards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| {
            !k["name"]
                .as_str()
                .unwrap()
                .starts_with("hl-virtual-keyboard")
        })
        .unwrap()["name"]
        .as_str()
        .unwrap();
    assert_eq!(
        ipc.request(&format!("switchxkblayout {physical_name} 1"))
            .unwrap()
            .trim(),
        "ok"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let devices = ipc.json("devices").unwrap();
        if devices["keyboards"].as_array().unwrap().iter().any(|k| {
            k["name"]
                .as_str()
                .unwrap()
                .starts_with("hl-virtual-keyboard")
                && k["active_keymap"]
                    .as_str()
                    .is_some_and(|name| name.contains("German"))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "activelayout did not refresh virtual group"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let offset = fixture.records().len();
    keyboard.key(HidUsage::keyboard(0x33), true).unwrap();
    keyboard.key(HidUsage::keyboard(0x33), false).unwrap();
    let records = fixture.wait(|r| keys(&r[offset..]).len() >= 2);
    assert_eq!(keys(&records[offset..])[0]["text"], "ö");
    eprintln!("activelayout: physical keyboard group 1 propagated to the existing virtual keymap");

    reload_layout(ipc, "us", "compose:caps");
    await_keymap(&connection, &mut queue, &mut events, |k| {
        k.layout_get_name(0).contains("English")
            && k.key_get_syms_by_level(66_u32.into(), 0, 0)
                .iter()
                .any(|s| s.raw() == xkb::keysyms::KEY_Multi_key)
    });
    let physical = keyboard.lock_keys().unwrap();
    let offset = fixture.records().len();
    let wanted = LockKeys {
        caps_lock: Some(!physical.caps_lock.unwrap()),
        num_lock: Some(!physical.num_lock.unwrap()),
        scroll_lock: Some(true),
    };
    keyboard.set_lock_keys(wanted).unwrap();
    let virtual_state = virtual_locks(ipc);
    assert_eq!(virtual_state.caps_lock, wanted.caps_lock);
    assert_eq!(virtual_state.num_lock, wanted.num_lock);
    assert_eq!(keyboard.lock_keys().unwrap(), physical_locks(ipc));
    keyboard.key(HidUsage::keyboard(4), true).unwrap();
    keyboard.key(HidUsage::keyboard(4), false).unwrap();
    let records = fixture.wait(|r| {
        keys(&r[offset..])
            .iter()
            .any(|e| e["code"] == "KeyA" && e["state"] == "released")
    });
    assert!(
        !keys(&records[offset..])
            .iter()
            .any(|e| e["code"] == "CapsLock"),
        "compose key was tapped as CapsLock"
    );
    assert_eq!(
        keys(&records[offset..])
            .iter()
            .find(|e| e["code"] == "KeyA" && e["state"] == "pressed")
            .unwrap()["text"],
        if wanted.caps_lock == Some(true) {
            "A"
        } else {
            "a"
        }
    );
    keyboard.set_lock_keys(physical).unwrap();
    assert_eq!(virtual_locks(ipc), physical);
    eprintln!(
        "runtime RMLVO: us→de→us/compose:caps; no per-key uploads, no compose tap; ScrollLock ignored"
    );

    let monitors = ipc.json("monitors").unwrap();
    let monitor = &monitors.as_array().unwrap()[0];
    let name = monitor["name"].as_str().unwrap();
    let display = DisplayId(monitor["id"].as_u64().unwrap() as u32);
    ipc.eval(&format!(
        "hl.monitor({{ output = {}, transform = 1 }})",
        serde_json::to_string(name).unwrap()
    ))
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let client = ipc.json("activewindow").unwrap();
    let scale = monitor["scale"].as_f64().unwrap();
    let ox = (client["at"][0].as_f64().unwrap() - monitor["x"].as_f64().unwrap()) * scale;
    let oy = (client["at"][1].as_f64().unwrap() - monitor["y"].as_f64().unwrap()) * scale;
    let sw = client["size"][0].as_f64().unwrap() * scale;
    let sh = client["size"][1].as_f64().unwrap() * scale;
    for (x, y) in [
        (sw / 4.0, sh / 4.0),
        (sw / 2.0, sh / 2.0),
        (sw * 0.75, sh * 0.75),
    ] {
        let offset = fixture.records().len();
        pointer
            .move_to(display, PointDevice::new(ox + x, oy + y))
            .unwrap();
        let records = fixture.wait(|r| r[offset..].iter().any(|e| e["event"] == "pointer"));
        let event = records[offset..]
            .iter()
            .rev()
            .find(|e| e["event"] == "pointer")
            .unwrap();
        assert!((event["x_px"].as_f64().unwrap() - x).abs() <= 1.0);
        assert!((event["y_px"].as_f64().unwrap() - y).abs() <= 1.0);
    }
    ipc.eval(&format!(
        "hl.monitor({{ output = {}, transform = 0 }})",
        serde_json::to_string(name).unwrap()
    ))
    .unwrap();
    eprintln!(
        "runtime output transform=1: all three post-transform device-pixel positions matched"
    );

    // Window-backed outputs allocate on this setup; headless outputs do not (P1a).
    let pointer_count = || {
        ipc.json("devices").unwrap()["mice"]
            .as_array()
            .unwrap()
            .len()
    };
    let initial_pointers = pointer_count();
    for _ in 0..2 {
        assert_eq!(
            ipc.request("output create wayland crosspane-inject-hotplug")
                .unwrap()
                .trim(),
            "ok"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let hotplug = loop {
            if let Some((_, id)) = ipc
                .monitor_ids()
                .unwrap()
                .iter()
                .find(|(name, _)| name == "crosspane-inject-hotplug")
            {
                break DisplayId(*id);
            }
            assert!(
                Instant::now() < deadline,
                "window-backed output never appeared"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        loop {
            match pointer.move_to(hotplug, PointDevice::new(0.0, 0.0)) {
                Ok(()) => break,
                Err(PlatformError::NotFound) => {}
                result => panic!("hotplug move failed: {result:?}"),
            }
            assert!(Instant::now() < deadline, "output's pointer never appeared");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pointer_count(), initial_pointers + 1);
        assert_eq!(
            ipc.request("output remove crosspane-inject-hotplug")
                .unwrap()
                .trim(),
            "ok"
        );
        loop {
            if matches!(
                pointer.move_to(hotplug, PointDevice::new(0.0, 0.0)),
                Err(PlatformError::NotFound)
            ) {
                break;
            }
            assert!(Instant::now() < deadline, "removed output kept its pointer");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pointer_count(), initial_pointers);
    }
    eprintln!(
        "output hotplug: two create/remove cycles; pointers created, destroyed and recreated"
    );
    drop(pointer);
    drop(keyboard);
}
