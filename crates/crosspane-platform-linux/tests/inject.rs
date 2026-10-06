//! WP-1.12/1.13 virtual input against a disposable nested Hyprland and the event fixture.
//! One test owns an isolated named nest, so default Nextest parallelism cannot steal its focus.
//! Raw destruction, our explicit Drop and recovery are checked sequentially in that nest.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
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
        held_lock.caps_lock,
        "a held Caps key must defer the requested mask until its real up"
    );
    keyboard.key(HidUsage::keyboard(0x39), false).unwrap();
    assert_eq!(
        virtual_locks(&ipc).caps_lock,
        Some(!held_lock.caps_lock.unwrap()),
        "the real up must finish before the requested mask is applied"
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
    refresh_failure_cases(&ipc);
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
        // Refresh alone publishes no modifier state. This fixture-only, unheld up triggers
        // the existing group's legitimate publication without creating a held key.
        keyboard.key(HidUsage::keyboard(0x73), false).unwrap();
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

/// A stand-in for Hyprland's request socket (`.socket.sock`) that forwards every request to the
/// nested instance, and on demand misbehaves, so the worker's configuration refresh fails or
/// returns something stale without any hook in the product code: the worker is connected through
/// a `HyprIpc` that points here. Every connection and forwarding thread is bounded and tracked;
/// dropping the stand-in closes them all and joins the threads.
struct FlakyIpc {
    runtime: PathBuf,
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

#[derive(Default)]
struct Shared {
    /// Close every new connection without a reply, so the request fails.
    failing: AtomicBool,
    /// Report a keymap layout that cannot compile in every `devices` reply.
    invalid_layout: AtomicBool,
    /// Hold each `monitors` reply (already read from Hyprland) until this is cleared, or until a
    /// permit releases it: each permit lets one held reply go.
    hold_monitors: AtomicBool,
    permits: AtomicUsize,
    /// Report this output under this monitor id in each `monitors` reply. Together with the hold,
    /// this is a snapshot that predates the output's recreation under another id.
    tamper: Mutex<Option<(String, u32)>>,
    held: AtomicUsize,
    delivered_monitors: AtomicUsize,
    rewritten: AtomicUsize,
    stop: AtomicBool,
    /// When each failing request arrived.
    refused: Mutex<Vec<Instant>>,
    /// Clones of every open connection, shut down when the stand-in is dropped. A forwarder
    /// removes its own when it finishes, so a closed connection is not kept open by its clone.
    open: Mutex<BTreeMap<u64, UnixStream>>,
    next: AtomicU64,
}

/// Registers a connection's clone in `Shared::open` until dropped.
struct Registered<'a> {
    shared: &'a Shared,
    id: u64,
}

impl<'a> Registered<'a> {
    fn new(shared: &'a Shared, stream: &UnixStream) -> Option<Self> {
        let id = shared.next.fetch_add(1, Ordering::AcqRel);
        shared
            .open
            .lock()
            .unwrap()
            .insert(id, stream.try_clone().ok()?);
        Some(Self { shared, id })
    }
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.shared.open.lock().unwrap().remove(&self.id);
    }
}

/// The longest any one forwarded request may take, in total.
const FORWARD_LIMIT: Duration = Duration::from_secs(3);

impl FlakyIpc {
    const SIGNATURE: &'static str = "flaky";

    fn start() -> Self {
        let upstream = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
            .join("hypr")
            .join(std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap());
        let runtime =
            std::env::temp_dir().join(format!("crosspane-wp-2-37-{}", std::process::id()));
        let directory = runtime.join("hypr").join(Self::SIGNATURE);
        fs::create_dir_all(&directory).unwrap();
        let shared = Arc::new(Shared::default());
        // Requests are forwarded (and made to fail on demand); events are relayed untouched, so
        // the worker's watcher hears the compositor's `activelayout`, `configreloaded` and
        // monitor events as it would directly.
        let threads = [(".socket.sock", false), (".socket2.sock", true)]
            .into_iter()
            .map(|(socket, events)| {
                let listener = UnixListener::bind(directory.join(socket)).unwrap();
                listener.set_nonblocking(true).unwrap();
                serve_socket(listener, upstream.join(socket), shared.clone(), events)
            })
            .collect();
        Self {
            runtime,
            shared,
            threads,
        }
    }

    fn ipc(&self) -> HyprIpc {
        // Longer than the longest hold below, so a held reply is never the client's timeout.
        HyprIpc::new(Self::SIGNATURE, &self.runtime, Duration::from_secs(5))
    }

    fn set_failing(&self, on: bool) {
        self.shared.failing.store(on, Ordering::Release);
    }

    fn set_invalid_layout(&self, on: bool) {
        self.shared.invalid_layout.store(on, Ordering::Release);
    }

    fn set_tamper(&self, tamper: Option<(&str, u32)>) {
        *self.shared.tamper.lock().unwrap() = tamper.map(|(name, id)| (name.to_owned(), id));
    }

    /// Let exactly one held `monitors` reply go; any later one stays held.
    fn release_one_held(&self) {
        self.shared.permits.fetch_add(1, Ordering::AcqRel);
    }

    fn set_hold_monitors(&self, on: bool) {
        self.shared.hold_monitors.store(on, Ordering::Release);
    }

    fn held(&self) -> usize {
        self.shared.held.load(Ordering::Acquire)
    }

    fn delivered_monitors(&self) -> usize {
        self.shared.delivered_monitors.load(Ordering::Acquire)
    }

    fn rewritten(&self) -> usize {
        self.shared.rewritten.load(Ordering::Acquire)
    }

    fn refused_times(&self) -> Vec<Instant> {
        self.shared.refused.lock().unwrap().clone()
    }
}

impl Drop for FlakyIpc {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        let _ = fs::remove_dir_all(&self.runtime);
    }
}

/// Accept on `listener` and serve each connection on its own tracked thread, until stopped.
/// `events` relays the compositor's event stream; otherwise requests are forwarded.
fn serve_socket(
    listener: UnixListener,
    upstream: PathBuf,
    shared: Arc<Shared>,
    events: bool,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut workers: Vec<JoinHandle<()>> = Vec::new();
        while !shared.stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((client, _)) => {
                    if !events && shared.failing.load(Ordering::Acquire) {
                        // Closing without a reply: the client's read ends empty or its write
                        // breaks, either way its request fails.
                        shared.refused.lock().unwrap().push(Instant::now());
                        continue;
                    }
                    let (upstream, shared) = (upstream.clone(), shared.clone());
                    workers.push(std::thread::spawn(move || {
                        if events {
                            relay_events(client, &upstream, &shared);
                        } else {
                            forward(client, &upstream, &shared);
                        }
                    }));
                    let (done, running) = workers.into_iter().partition(|w| w.is_finished());
                    workers = running;
                    for worker in done {
                        worker.join().unwrap();
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => panic!("stand-in IPC socket: {e}"),
            }
        }
        // Close what is still open so no forwarder waits out its limit, then join them all.
        for stream in shared.open.lock().unwrap().values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        for worker in workers {
            worker.join().unwrap();
        }
    })
}

/// Pass the compositor's event stream through byte for byte until either end closes or the
/// stand-in stops.
fn relay_events(client: UnixStream, upstream: &Path, shared: &Shared) {
    let connected = connect_within(upstream, Instant::now() + FORWARD_LIMIT);
    if let Some(mut source) = connected
        && let Some(_source_open) = Registered::new(shared, &source)
        && let Some(_client_open) = Registered::new(shared, &client)
        && source
            .set_read_timeout(Some(Duration::from_millis(50)))
            .is_ok()
        && client
            .set_write_timeout(Some(Duration::from_secs(1)))
            .is_ok()
    {
        let mut sink = &client;
        let mut chunk = [0_u8; 4096];
        while !shared.stop.load(Ordering::Acquire) {
            match source.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if sink.write_all(&chunk[..n]).is_err() {
                        break;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => break,
            }
        }
    }
    let _ = client.shutdown(Shutdown::Both);
}

/// Connect to a Unix socket without blocking past `deadline`.
fn connect_within(path: &Path, deadline: Instant) -> Option<UnixStream> {
    let address = rustix::net::SocketAddrUnix::new(path).ok()?;
    let fd = rustix::net::socket_with(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
        None,
    )
    .ok()?;
    loop {
        match rustix::net::connect(&fd, &address) {
            Ok(()) => break,
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return None,
        }
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(false).ok()?;
    Some(stream)
}

fn forward(client: UnixStream, real: &Path, shared: &Shared) {
    serve(&client, real, shared);
    // Closed explicitly: the client waits for the end of the reply.
    let _ = client.shutdown(Shutdown::Both);
}

fn serve(mut client: &UnixStream, real: &Path, shared: &Shared) {
    let deadline = Instant::now() + FORWARD_LIMIT;
    let left = || {
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1))
    };
    if shared.stop.load(Ordering::Acquire) || client.set_nonblocking(false).is_err() {
        return;
    }
    let Some(_client_registered) = Registered::new(shared, client) else {
        return;
    };
    if client.set_read_timeout(Some(left())).is_err() {
        return;
    }
    let mut request = [0_u8; 4096];
    let Ok(length) = client.read(&mut request) else {
        return;
    };
    let request = &request[..length];
    let Some(mut upstream) = connect_within(real, deadline) else {
        return;
    };
    let Some(_upstream_registered) = Registered::new(shared, &upstream) else {
        return;
    };
    if upstream.set_write_timeout(Some(left())).is_err() || upstream.write_all(request).is_err() {
        return;
    }
    let mut reply = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        if Instant::now() >= deadline || upstream.set_read_timeout(Some(left())).is_err() {
            return;
        }
        match upstream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => reply.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
    if request.starts_with(b"j/devices")
        && shared.invalid_layout.load(Ordering::Acquire)
        && let Ok(mut devices) = serde_json::from_slice::<Value>(&reply)
    {
        for keyboard in devices["keyboards"].as_array_mut().into_iter().flatten() {
            keyboard["layout"] = Value::from("no-such-layout");
        }
        reply = serde_json::to_vec(&devices).unwrap();
        shared.rewritten.fetch_add(1, Ordering::AcqRel);
    }
    let monitors = request.starts_with(b"j/monitors");
    if monitors
        && let Some((name, id)) = shared.tamper.lock().unwrap().clone()
        && let Ok(mut list) = serde_json::from_slice::<Value>(&reply)
    {
        for monitor in list.as_array_mut().into_iter().flatten() {
            if monitor["name"] == name.as_str() {
                monitor["id"] = Value::from(id);
            }
        }
        reply = serde_json::to_vec(&list).unwrap();
    }
    if monitors && shared.hold_monitors.load(Ordering::Acquire) {
        shared.held.fetch_add(1, Ordering::AcqRel);
        let hold_until = Instant::now() + Duration::from_secs(4);
        while shared.hold_monitors.load(Ordering::Acquire)
            && !shared.stop.load(Ordering::Acquire)
            && Instant::now() < hold_until
            && shared
                .permits
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_err()
        {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    if client
        .set_write_timeout(Some(Duration::from_secs(1)))
        .is_ok()
        && client.write_all(&reply).is_ok()
        && monitors
    {
        shared.delivered_monitors.fetch_add(1, Ordering::AcqRel);
    }
}

fn wait_for(what: &str, done: &mut dyn FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn output_id(ipc: &HyprIpc, name: &str) -> Option<u32> {
    ipc.monitor_ids()
        .unwrap()
        .iter()
        .find(|(n, _)| n == name)
        .map(|&(_, id)| id)
}

fn create_output(ipc: &HyprIpc, name: &str) -> u32 {
    assert_eq!(
        ipc.request(&format!("output create wayland {name}"))
            .unwrap()
            .trim(),
        "ok"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(id) = output_id(ipc, name) {
            return id;
        }
        assert!(
            Instant::now() < deadline,
            "output {name} never appeared; outputs are {:?}",
            ipc.monitor_ids()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn remove_output(ipc: &HyprIpc, name: &str) {
    assert_eq!(
        ipc.request(&format!("output remove {name}"))
            .unwrap()
            .trim(),
        "ok"
    );
    wait_for("output disappears", &mut || output_id(ipc, name).is_none());
}

fn mice(ipc: &HyprIpc) -> usize {
    ipc.json("devices").unwrap()["mice"]
        .as_array()
        .unwrap()
        .len()
}

/// Wait until exactly `expected` pointer devices exist; on timeout say which ones do.
fn wait_for_mice(ipc: &HyprIpc, expected: usize, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while mice(ipc) != expected {
        assert!(
            Instant::now() < deadline,
            "timed out: {what}: {} pointer devices, expected {expected}: {}",
            mice(ipc),
            ipc.json("devices").unwrap()["mice"]
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn rotate(ipc: &HyprIpc, name: &str, transform: u32) {
    ipc.eval(&format!(
        "hl.monitor({{ output = {name:?}, transform = {transform} }})"
    ))
    .unwrap();
}

/// Motion on the output's pointer is accepted and moves the cursor onto that output. Only
/// transient outcomes are retried: the pointer is not bound yet (`NotFound`), the worker is
/// still paused (`Timeout`), or the output is mid-resize (`Unsupported`: it is a window in the
/// parent compositor, which keeps re-tiling it, and the aim is the middle of its current size).
/// Any other outcome fails the test.
fn motion_reaches_output(pointer: &mut impl PointerInjector, ipc: &HyprIpc, name: &str, id: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let (monitor, at) = loop {
        let monitors = ipc.json("monitors").unwrap();
        let monitor = monitors
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .unwrap()
            .clone();
        let at = (
            monitor["width"].as_f64().unwrap() / 2.0,
            monitor["height"].as_f64().unwrap() / 2.0,
        );
        let last = match pointer.move_to(DisplayId(id), PointDevice::new(at.0, at.1)) {
            Ok(()) => break (monitor, at),
            Err(e @ (PlatformError::NotFound | PlatformError::Timeout)) => e,
            Err(e @ PlatformError::Unsupported(_)) => e,
            Err(e) => panic!("motion to {name} failed: {e:?}"),
        };
        assert!(
            Instant::now() < deadline,
            "motion to {name} was never accepted: {last:?}, output {monitor}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let scale = monitor["scale"].as_f64().unwrap();
    let wanted = (
        monitor["x"].as_f64().unwrap() + at.0 / scale,
        monitor["y"].as_f64().unwrap() + at.1 / scale,
    );
    wait_for("the cursor reaches the output", &mut || {
        let cursor = ipc.json("cursorpos").unwrap();
        (cursor["x"].as_f64().unwrap() - wanted.0).abs() <= 3.0
            && (cursor["y"].as_f64().unwrap() - wanted.1).abs() <= 3.0
    });
}

/// The refresh attempts after the leading burst. An output change makes the worker ask for a
/// refresh itself, several times in quick succession; those are within 100 ms of the first
/// attempt. What follows are retries.
fn retries_after_the_burst(attempts: &[Instant]) -> Vec<Instant> {
    let Some(&first) = attempts.first() else {
        return Vec::new();
    };
    attempts
        .iter()
        .copied()
        .filter(|t| t.duration_since(first) >= Duration::from_millis(100))
        .collect()
}

/// Retries keep happening, never faster than the 100 ms minimum, and never stop for long. Which
/// step of the backoff each one is cannot be told from real timing: the exact schedule is
/// asserted on a controlled clock in the unit tests (`wayland::tests`).
fn assert_retries_keep_happening(attempts: &[Instant]) {
    let retries = retries_after_the_burst(attempts);
    assert!(retries.len() >= 4, "too few retries: {}", retries.len());
    for pair in retries.windows(2) {
        let gap = pair[1].duration_since(pair[0]);
        assert!(
            gap >= Duration::from_millis(100),
            "retries {gap:?} apart, faster than the 100 ms minimum"
        );
        assert!(
            gap <= Duration::from_millis(2500),
            "a retry was {gap:?} late, past the 2 s cap"
        );
    }
}

/// The centre of the fixture's window in its output's device pixels. A new output re-tiles the
/// window for a while, so wait until its geometry has not changed for 400 ms.
fn settled_window_centre(ipc: &HyprIpc, pid: u32, display: DisplayId) -> PointDevice {
    let geometry = || {
        let client = ipc.json("activewindow").unwrap();
        assert_eq!(client["pid"].as_u64(), Some(u64::from(pid)));
        let monitors = ipc.json("monitors").unwrap();
        let monitor = monitors
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"].as_u64() == Some(u64::from(display.0)))
            .unwrap()
            .clone();
        (client, monitor)
    };
    let started = Instant::now();
    let mut last = geometry();
    let mut since = started;
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let now = geometry();
        if now != last {
            (last, since) = (now, Instant::now());
        } else if since.elapsed() >= Duration::from_millis(400) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "window geometry never settled"
        );
    }
    let (client, monitor) = last;
    let number = |value: &Value| value.as_f64().unwrap();
    // The window can be larger than a small output: aim at the middle of the part that shows.
    let middle = |at: f64, size: f64, origin: f64, extent: f64| {
        let (low, high) = (at.max(origin), (at + size).min(origin + extent));
        assert!(low < high, "the window is not on the output");
        (low + high) / 2.0 - origin
    };
    let scale = number(&monitor["scale"]);
    PointDevice::new(
        middle(
            number(&client["at"][0]),
            number(&client["size"][0]),
            number(&monitor["x"]),
            number(&monitor["width"]) / scale,
        ) * scale,
        middle(
            number(&client["at"][1]),
            number(&client["size"][1]),
            number(&monitor["y"]),
            number(&monitor["height"]) / scale,
        ) * scale,
    )
}

const OUTPUT_A: &str = "crosspane-wp-2-37-a";
const OUTPUT_B: &str = "crosspane-wp-2-37-b";
const OUTPUT_C: &str = "crosspane-wp-2-37-c";
const F24: HidUsage = HidUsage::keyboard(0x73);

/// The refresh-failure cases, one worker each, sharing three extra outputs.
///
/// Outputs are only ever created while the cases run and are all removed at the very end: the
/// nested compositor often cannot create another output for a long while after one that lived a
/// few seconds has been removed, which would fail whichever case came next.
fn refresh_failure_cases(ipc: &HyprIpc) {
    failed_refresh_pauses_then_resumes(ipc);
    // The cases below rotate the outputs; start them upright.
    for name in [OUTPUT_A, OUTPUT_B] {
        rotate(ipc, name, 0);
    }
    a_keyboard_change_while_a_paused_refresh_is_in_flight(ipc);
    a_stale_snapshot_is_not_trusted(ipc, OUTPUT_A, None);
    a_stale_snapshot_is_not_trusted(ipc, OUTPUT_A, Some(OUTPUT_C));
    for name in [OUTPUT_A, OUTPUT_B, OUTPUT_C] {
        remove_output(ipc, name);
    }
}

/// The number of pointer devices that are not this test's: wait until the previous worker's have
/// gone.
fn settled_mice(ipc: &HyprIpc) -> usize {
    let mut last = (mice(ipc), Instant::now());
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let now = mice(ipc);
        if now != last.0 {
            last = (now, Instant::now());
        } else if last.1.elapsed() >= Duration::from_millis(400) {
            return now;
        }
    }
}

/// WP-2.37: one failed configuration refresh must not kill injection for good. It pauses it: held
/// input is released, new input is refused with the retryable `Timeout` (releases still work), no
/// pointer is bound from the output map that is now unknown, and the worker keeps retrying with
/// backoff until a refresh succeeds, when input resumes against the fresh configuration. A closed
/// gate is independent of all of this.
fn failed_refresh_pauses_then_resumes(ipc: &HyprIpc) {
    reload_layout(ipc, "us", "");
    let base = settled_mice(ipc);
    let outputs = || ipc.monitor_ids().unwrap().len();
    let flaky = FlakyIpc::start();
    let gate = open_gate();
    let (mut keyboard, mut pointer) = connect(gate.clone(), flaky.ipc()).unwrap();
    let fixture = Fixture::start(ipc);
    // A position that is inside any output: refusals are asserted with it, since the output may
    // be re-tiled to a different size meanwhile and a position outside it is `Unsupported`.
    let corner = PointDevice::new(1.0, 1.0);
    let monitors = ipc.json("monitors").unwrap();
    let monitor = &monitors.as_array().unwrap()[0];
    let display = DisplayId(u32::try_from(monitor["id"].as_u64().unwrap()).unwrap());
    wait_for_mice(ipc, base + outputs(), "a pointer for the main output");

    // Running: an output's pointer is bound through the (still healthy) refresh path.
    create_output(ipc, OUTPUT_A);
    wait_for_mice(ipc, base + outputs(), "a pointer for the new output");

    // Hold a key and a button over the fixture window, once the new output has stopped moving it.
    let centre = settled_window_centre(ipc, fixture.process.id(), display);
    pointer.move_to(display, centre).unwrap();
    let button_events = |state: &str| {
        fixture
            .records()
            .iter()
            .filter(|e| e["event"] == "button" && e["button"] == 1 && e["state"] == state)
            .count()
    };
    keyboard.key(F24, true).unwrap();
    pointer.button(MouseButton::PRIMARY, true).unwrap();
    wait_for("held key and button arrive", &mut || {
        fixture.key_count("pressed") == 1 && button_events("pressed") == 1
    });
    assert_eq!(
        (fixture.key_count("released"), button_events("released")),
        (0, 0)
    );

    // The refresh fails: an output change (a rotation of the extra output, which leaves the
    // fixture's window alone) makes the worker ask for one, and the stand-in drops that request.
    // The pause releases the held key and button on its own.
    flaky.set_failing(true);
    rotate(ipc, OUTPUT_A, 1);
    wait_for("pause releases the held key and button", &mut || {
        fixture.key_count("released") == 1 && button_events("released") == 1
    });

    // Autonomous retries. From here to the check nothing is sent to the worker and no output
    // changes: the requests it makes are its own. Whatever event-driven requests the rotation
    // caused arrive in a burst first and drain; then only the retries are left.
    wait_for("four retries after the burst", &mut || {
        retries_after_the_burst(&flaky.refused_times()).len() >= 4
    });
    assert_retries_keep_happening(&flaky.refused_times());

    // Paused: every injection is refused as retryable, nothing reaches the app.
    assert!(matches!(
        keyboard.key(F24, true),
        Err(PlatformError::Timeout)
    ));
    assert!(matches!(
        pointer.move_to(display, corner),
        Err(PlatformError::Timeout)
    ));
    assert!(matches!(
        pointer.button(MouseButton::PRIMARY, true),
        Err(PlatformError::Timeout)
    ));
    assert!(matches!(
        pointer.scroll(wheel(0, 120)),
        Err(PlatformError::Timeout)
    ));
    // Releases and recovery are still honoured, so nothing can stay down.
    keyboard.key(F24, false).unwrap();
    pointer.button(MouseButton::PRIMARY, false).unwrap();
    keyboard.release_all().unwrap();
    pointer.release_all().unwrap();
    keyboard.recover_keys(&[F24]).unwrap();
    pointer.recover_buttons(&[MouseButton::PRIMARY]).unwrap();

    // An output that appears while paused is not in the map the worker holds, so it gets no
    // pointer until a refresh succeeds.
    let pointers = mice(ipc);
    create_output(ipc, OUTPUT_B);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(mice(ipc), pointers, "a pointer was bound while paused");
    assert!(matches!(
        keyboard.key(F24, true),
        Err(PlatformError::Timeout)
    ));
    assert_eq!(
        (
            fixture.key_count("pressed"),
            fixture.key_count("released"),
            button_events("pressed"),
            button_events("released"),
        ),
        (1, 1, 1, 1),
        "input reached the app while paused"
    );

    // The gate is independent of the pause and wins over it: closed while paused, the answer is
    // `Locked`, not the retryable `Timeout`.
    gate.set_engine_permits(false);
    let assert_locked = |keyboard: &mut dyn KeyInjector, pointer: &mut dyn PointerInjector| {
        assert!(matches!(
            keyboard.key(F24, true),
            Err(PlatformError::Locked)
        ));
        assert!(matches!(
            pointer.move_to(display, corner),
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
    };
    assert_locked(&mut keyboard, &mut pointer);

    // The configuration recovers while the gate is still closed. A refresh succeeds and is
    // applied (the worker is no longer paused), yet input stays refused, now as `Locked`.
    let before = flaky.delivered_monitors();
    flaky.set_failing(false);
    wait_for("a refresh succeeds", &mut || {
        flaky.delivered_monitors() > before
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_locked(&mut keyboard, &mut pointer);
    assert_eq!(
        (fixture.key_count("pressed"), button_events("pressed")),
        (1, 1),
        "input reached the app through a closed gate"
    );

    // Reopening the gate is enough: the very next call works, with no waiting for the refresh,
    // which proves the worker resumed while the gate was closed.
    // (Creating the output re-tiled the window, so aim again first.)
    let centre = settled_window_centre(ipc, fixture.process.id(), display);
    gate.set_engine_permits(true);
    keyboard.key(F24, true).unwrap();
    keyboard.key(F24, false).unwrap();
    pointer.move_to(display, centre).unwrap();
    pointer.button(MouseButton::PRIMARY, true).unwrap();
    pointer.button(MouseButton::PRIMARY, false).unwrap();
    wait_for("input after resuming reaches the app", &mut || {
        fixture.key_count("pressed") == 2
            && fixture.key_count("released") == 2
            && button_events("pressed") == 2
            && button_events("released") == 2
    });

    // The output that appeared meanwhile now has a pointer, bound from the fresh map, and its
    // motion reaches it.
    wait_for_mice(
        ipc,
        base + outputs(),
        "a pointer for the output that appeared",
    );
    let appeared = output_id(ipc, OUTPUT_B).unwrap();
    motion_reaches_output(&mut pointer, ipc, OUTPUT_B, appeared);

    // A configuration whose keymap does not compile is a failed refresh too: the worker pauses,
    // keeps retrying, and resumes only when a valid configuration arrives.
    let pressed = fixture.key_count("pressed");
    flaky.set_invalid_layout(true);
    rotate(ipc, OUTPUT_B, 1);
    wait_for("an invalid configuration is delivered", &mut || {
        flaky.rewritten() >= 1
    });
    let served = flaky.rewritten();
    wait_for("it is delivered again on a retry", &mut || {
        flaky.rewritten() >= served + 2
    });
    assert!(matches!(
        keyboard.key(F24, true),
        Err(PlatformError::Timeout)
    ));
    assert!(matches!(
        pointer.move_to(display, corner),
        Err(PlatformError::Timeout)
    ));
    flaky.set_invalid_layout(false);
    wait_for(
        "input resumes with a valid configuration",
        &mut || match keyboard.key(F24, true) {
            Ok(()) => true,
            Err(PlatformError::Timeout) => false,
            Err(e) => panic!("unexpected: {e:?}"),
        },
    );
    keyboard.key(F24, false).unwrap();
    wait_for("the resumed key reaches the app", &mut || {
        fixture.key_count("pressed") == pressed + 1 && fixture.key_count("released") == pressed + 1
    });

    eprintln!(
        "failed refresh: held input released, retries on the backoff schedule, Timeout while paused, \
         releases honoured, Locked wins and is independent, no pointer bound while paused, an invalid \
         keymap keeps it paused, resumed on the next good refresh"
    );
    drop(pointer);
    drop(keyboard);
}

/// WP-2.37: a snapshot that predates an output change must not resume a paused worker, and a
/// pointer bound from it under the wrong monitor id must not outlive the next fresh snapshot.
///
/// The stale snapshot is made, not waited for: the stand-in holds a `monitors` reply that was
/// read before the change, with `shown` reported under an id it does not have (as after being
/// recreated under another id), and delivers it after the change. (Recreating an output is
/// unreliable in the nested compositor, so this does not depend on it.)
///
/// With `added`, the worker is paused when the snapshot arrives (and `added` appears while it
/// is): it must stay paused, bind nothing from the snapshot, and resume on the fresh one.
/// Otherwise the worker is running, applies the stale snapshot, and must correct its binding on
/// the fresh one.
fn a_stale_snapshot_is_not_trusted(ipc: &HyprIpc, shown: &str, added: Option<&str>) {
    const WRONG_ID: u32 = 90;
    let paused = added.is_some();
    reload_layout(ipc, "us", "");
    let base = settled_mice(ipc);
    let outputs = || ipc.monitor_ids().unwrap().len();
    let flaky = FlakyIpc::start();
    let (mut keyboard, mut pointer) = connect(open_gate(), flaky.ipc()).unwrap();
    wait_for_mice(ipc, base + outputs(), "a pointer for every output");
    let shown_id = output_id(ipc, shown).unwrap();
    let before = outputs();

    let mut added_id = None;
    if let Some(added) = added {
        // The refresh fails after an output change, so the worker pauses. An output that appears
        // meanwhile is not in the map the worker holds.
        flaky.set_failing(true);
        rotate(ipc, shown, 1);
        wait_for("the worker pauses", &mut || match keyboard.key(F24, true) {
            Ok(()) => {
                keyboard.key(F24, false).unwrap();
                false
            }
            Err(PlatformError::Timeout) => true,
            Err(e) => panic!("unexpected: {e:?}"),
        });
        added_id = Some(create_output(ipc, added));
        std::thread::sleep(Duration::from_millis(300));
    }

    // A read starts, and its `monitors` reply, which has `shown` under the wrong id, is held.
    flaky.set_tamper(Some((shown, WRONG_ID)));
    flaky.set_hold_monitors(true);
    if paused {
        flaky.set_failing(false);
    } else {
        rotate(ipc, shown, 1);
    }
    wait_for("a snapshot is held", &mut || flaky.held() >= 1);
    flaky.set_tamper(None);

    // An output change the held read cannot have seen.
    rotate(ipc, shown, 0);
    std::thread::sleep(Duration::from_millis(300));

    let delivered = flaky.delivered_monitors();
    if paused {
        // The stale snapshot is delivered, but the read after it is held, so no fresh one
        // arrives yet. The worker must stay paused and bind nothing from it: `shown`'s pointer,
        // whose id the snapshot contradicts, is gone, it is not rebound under the wrong id, and
        // the output that appeared while paused has none.
        flaky.release_one_held();
        wait_for("the stale snapshot is delivered", &mut || {
            flaky.delivered_monitors() > delivered
        });
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            matches!(keyboard.key(F24, true), Err(PlatformError::Timeout)),
            "a stale snapshot resumed the worker"
        );
        assert_eq!(
            mice(ipc),
            base + before - 1,
            "a pointer was bound while paused, or kept from the stale snapshot"
        );
        // A snapshot read after the change is allowed through.
        flaky.set_hold_monitors(false);
        wait_for("a fresh snapshot is delivered", &mut || {
            flaky.delivered_monitors() > delivered + 1
        });
    } else {
        flaky.set_hold_monitors(false);
        // The stale snapshot, then the fresh one the output change asked for.
        wait_for(
            "the stale and then a fresh snapshot are delivered",
            &mut || flaky.delivered_monitors() >= delivered + 2,
        );
    }
    // Let the worker apply it.
    std::thread::sleep(Duration::from_millis(300));

    // The fresh snapshot binds every output under its own id, and the wrong id has no pointer.
    motion_reaches_output(&mut pointer, ipc, shown, shown_id);
    if let (Some(added), Some(id)) = (added, added_id) {
        motion_reaches_output(&mut pointer, ipc, added, id);
    }
    wait_for_mice(
        ipc,
        base + outputs(),
        "one pointer per output, none from the stale snapshot",
    );
    let wrong = pointer.move_to(DisplayId(WRONG_ID), PointDevice::new(0.0, 0.0));
    assert!(
        matches!(wrong, Err(PlatformError::NotFound)),
        "a pointer is bound under the stale id {WRONG_ID}: {wrong:?}"
    );
    keyboard.key(F24, true).unwrap();
    keyboard.key(F24, false).unwrap();

    eprintln!(
        "stale snapshot ({}): {shown} reported as id {WRONG_ID} (really {shown_id}) after an output \
         change; handled by the fresh snapshot",
        if paused { "paused" } else { "running" }
    );
    drop(pointer);
    drop(keyboard);
}

/// WP-2.37: a snapshot whose keyboard configuration was read before a layout change must not
/// resume a paused worker, even though no output changed. The change is a layout switch on a
/// two-layout keyboard (`activelayout`, with no config reload and no output event), which the
/// worker's watcher hears as a compositor event (relayed by the stand-in) and which has to count
/// against the snapshot.
fn a_keyboard_change_while_a_paused_refresh_is_in_flight(ipc: &HyprIpc) {
    // HID usage 0x33 types `;` in the US layout and `ö` in the German one.
    const SEMICOLON_KEY: HidUsage = HidUsage::keyboard(0x33);
    reload_layout(ipc, "us,de", "");
    let physical = ipc.json("devices").unwrap()["keyboards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["name"].as_str().unwrap().to_owned())
        .find(|name| !name.starts_with("hl-virtual-keyboard"))
        .unwrap();
    let flaky = FlakyIpc::start();
    let (mut keyboard, _pointer) = connect(open_gate(), flaky.ipc()).unwrap();
    let fixture = Fixture::start(ipc);
    let typed = |text: &str| {
        keys(&fixture.records())
            .iter()
            .any(|e| e["state"] == "pressed" && e["text"] == text)
    };

    // The refresh fails after an output change, so the worker pauses.
    flaky.set_failing(true);
    rotate(ipc, OUTPUT_A, 1);
    wait_for("the worker pauses", &mut || match keyboard.key(F24, true) {
        Ok(()) => {
            keyboard.key(F24, false).unwrap();
            false
        }
        Err(PlatformError::Timeout) => true,
        Err(e) => panic!("unexpected: {e:?}"),
    });

    // A retry reads the keyboard configuration as it is now (the first layout, US), and its
    // reply is held...
    flaky.set_hold_monitors(true);
    flaky.set_failing(false);
    wait_for("a snapshot is held", &mut || flaky.held() >= 1);
    // ...while the keyboard switches to the second layout (German). The outputs do not change.
    assert_eq!(
        ipc.request(&format!("switchxkblayout {physical} 1"))
            .unwrap()
            .trim(),
        "ok"
    );
    std::thread::sleep(Duration::from_millis(400));

    // The old snapshot is delivered, but the read after it is held, so no fresh one arrives yet.
    let delivered = flaky.delivered_monitors();
    flaky.release_one_held();
    wait_for("the stale snapshot is delivered", &mut || {
        flaky.delivered_monitors() > delivered
    });
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        matches!(
            keyboard.key(SEMICOLON_KEY, true),
            Err(PlatformError::Timeout)
        ),
        "a keyboard snapshot read before the layout switch resumed injection"
    );

    // A read that began after the switch is allowed through: the worker resumes in the new group.
    flaky.set_hold_monitors(false);
    wait_for(
        "input resumes",
        &mut || match keyboard.key(SEMICOLON_KEY, true) {
            Ok(()) => true,
            Err(PlatformError::Timeout) => false,
            Err(e) => panic!("unexpected: {e:?}"),
        },
    );
    keyboard.key(SEMICOLON_KEY, false).unwrap();
    wait_for("the key reaches the app", &mut || typed("ö"));
    assert!(!typed(";"), "a key was typed with the old layout");

    reload_layout(ipc, "us", "");
    rotate(ipc, OUTPUT_A, 0);
    eprintln!("keyboard switch during a paused refresh: the old snapshot did not resume it");
    drop(keyboard);
}
