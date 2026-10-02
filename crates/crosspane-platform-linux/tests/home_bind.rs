//! The home bind in a nested Hyprland only (WP-2.43f). The tests that need a compositor skip with
//! a message unless `CROSSPANE_NESTED_HYPR=1` (from `eval "$(scripts/hypr-nested.sh env)"`); they
//! install keybinds and inject keys, so they never run against the owner's session. When the
//! variable is set they also verify, before touching anything, that the endpoints they would reach
//! are the nest's (`verify_nest`: no inherited `WAYLAND_SOCKET`, and the signature and Wayland
//! socket belong to one running instance recorded by `scripts/hypr-nested.sh`) and **fail** if not.
//! `nest_guard_refuses_inherited_sockets_and_foreign_endpoints`, `unsupported_key_refused` and
//! `quoting_refused` need no compositor and always run.
//!
//! The nest's own config binds `CTRL + ALT + Escape` to "exit the session". The tests never press
//! that chord: the home bind's chord has one more modifier, and Hyprland matches modifier masks
//! exactly (`virtual_chord_fires_bind` pins that).
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crosspane_platform::{Chord, PlatformError};
use crosspane_platform_linux::hyprland::home_bind::{DESCRIPTION, HomeBind};
use crosspane_platform_linux::hyprland::ipc::HyprIpc;
use crosspane_types::hid::HidUsage;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use serde_json::Value;
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::{
    wp::keyboard_shortcuts_inhibit::zv1::client::{
        zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
        zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
    },
    xdg::shell::client::{
        xdg_surface::{self, XdgSurface},
        xdg_toplevel::XdgToplevel,
        xdg_wm_base::{self, XdgWmBase},
    },
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use xkbcommon::xkb;

/// The default release chord, in Hyprland's spelling and as `binds -j` lists it.
const CHORD_KEYS: &str = "CTRL + SHIFT + ALT + Escape";
const CHORD_MODMASK: u64 = 13;
/// A second chord for control binds that must not fire.
const CONTROL_KEYS: &str = "CTRL + SHIFT + ALT + F11";
/// Evdev codes the virtual keyboard sends.
const KEY_ESC: u32 = 1;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTALT: u32 = 56;
const KEY_F11: u32 = 87;
const CHORD_MODS: [u32; 3] = [KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_LEFTALT];
/// Time for a fork from the compositor to leave a mark, and for a bind that should not fire to
/// have fired.
const FIRE_TIMEOUT: Duration = Duration::from_secs(3);
const QUIET: Duration = Duration::from_millis(600);

/// Whether the compositor tests run. Without `CROSSPANE_NESTED_HYPR=1` they skip with a message.
/// With it, every endpoint the process would reach is verified to belong to a nest started by
/// `scripts/hypr-nested.sh` ([`verify_nest`]) and the test **panics** if not: these tests install
/// keybinds and inject keys, so a wrong endpoint must never be talked to, not merely skipped.
fn nested() -> bool {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!(
            "skipped: the home bind tests need CROSSPANE_NESTED_HYPR=1 from scripts/hypr-nested.sh env"
        );
        return false;
    }
    require_nest();
    true
}

/// Panic unless the process environment addresses a nested Hyprland.
fn require_nest() {
    if let Err(why) = verify_nest(&Endpoints::from_process()) {
        panic!(
            "refusing to run against a compositor that is not a nest from scripts/hypr-nested.sh: {why}"
        );
    }
}

/// What the environment says about the compositors this process would reach.
struct Endpoints {
    runtime_dir: PathBuf,
    /// `wayland_client::Connection::connect_to_env` prefers an inherited `WAYLAND_SOCKET` (a
    /// file descriptor) over `WAYLAND_DISPLAY`.
    wayland_socket: Option<std::ffi::OsString>,
    wayland_display: Option<String>,
    signature: Option<String>,
}

impl Endpoints {
    fn from_process() -> Endpoints {
        Endpoints {
            runtime_dir: PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default()),
            wayland_socket: std::env::var_os("WAYLAND_SOCKET"),
            wayland_display: std::env::var("WAYLAND_DISPLAY").ok(),
            signature: std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok(),
        }
    }
}

/// `Ok` only if the Wayland and IPC endpoints are the ones of one running nest.
///
/// 1. `WAYLAND_SOCKET` must be unset.
/// 2. Hyprland's own `hyprland.lock` for the signature (its pid and its Wayland socket name) must
///    name the `WAYLAND_DISPLAY` we would connect to.
/// 3. `$XDG_RUNTIME_DIR/crosspane-hypr-<name>/` (written by `scripts/hypr-nested.sh start`) must
///    record the same signature and Wayland socket, and the pid of that same, still running
///    compositor. The live session has no such directory, so its signature is refused.
fn verify_nest(e: &Endpoints) -> Result<(), String> {
    if let Some(fd) = &e.wayland_socket {
        return Err(format!(
            "WAYLAND_SOCKET={fd:?} is set; wayland-client would connect to that inherited socket instead of \
             WAYLAND_DISPLAY. Use `eval \"$(scripts/hypr-nested.sh env)\"`, which unsets it"
        ));
    }
    let display = e
        .wayland_display
        .as_deref()
        .filter(|d| !d.is_empty())
        .ok_or("WAYLAND_DISPLAY is not set")?;
    let signature = e
        .signature
        .as_deref()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .ok_or("HYPRLAND_INSTANCE_SIGNATURE is not set or not a plain signature")?;
    let lock_path = e
        .runtime_dir
        .join("hypr")
        .join(signature)
        .join("hyprland.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .map_err(|err| format!("can't read {}: {err}", lock_path.display()))?;
    let mut lock = lock.lines();
    let lock_pid = lock.next().unwrap_or_default().trim();
    let lock_display = lock.next().unwrap_or_default().trim();
    if lock_pid.is_empty()
        || !lock_pid.bytes().all(|b| b.is_ascii_digit())
        || lock_display != display
    {
        return Err(format!(
            "instance {signature} serves {lock_display:?}, but WAYLAND_DISPLAY is {display:?}"
        ));
    }
    let states = std::fs::read_dir(&e.runtime_dir)
        .map_err(|err| format!("can't list {}: {err}", e.runtime_dir.display()))?;
    for state in states.flatten() {
        let is_nest_dir = state
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("crosspane-hypr-"));
        if !is_nest_dir {
            continue;
        }
        let env = std::fs::read_to_string(state.path().join("env")).unwrap_or_default();
        let pid = std::fs::read_to_string(state.path().join("pid")).unwrap_or_default();
        let exported = |name: &str| {
            env.lines().find_map(|line| {
                line.strip_prefix("export ")?
                    .strip_prefix(name)?
                    .strip_prefix('=')
            })
        };
        if exported("HYPRLAND_INSTANCE_SIGNATURE") == Some(signature)
            && exported("WAYLAND_DISPLAY") == Some(display)
            && pid.trim() == lock_pid
            && Path::new("/proc").join(lock_pid).exists()
        {
            return Ok(());
        }
    }
    Err(format!(
        "no running nest started by scripts/hypr-nested.sh owns instance {signature} on {display} \
         (is this the live session?)"
    ))
}

/// Nextest runs tests in separate processes, so the shared nest is protected with a file lock.
fn serialize() -> File {
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap();
    let path = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
        .join(format!("crosspane-home-bind-{signature}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
    lock
}

fn chord(modifiers: &[u16], key: u16) -> Chord {
    Chord {
        modifiers: modifiers.iter().map(|m| HidUsage::keyboard(*m)).collect(),
        key: HidUsage::keyboard(key),
    }
}

/// The engine's default release chord: Left Ctrl, Left Shift, Left Alt and Escape.
fn default_chord() -> Chord {
    chord(&[0xE0, 0xE1, 0xE2], 0x29)
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One test's view of the nest: its lock, a scratch directory whose name contains a space (the
/// bind's command quotes it), a stub command that appends a line to a marker file per press, and a
/// guard that leaves the nest as it found it.
struct Fixture {
    _lock: File,
    ipc: HyprIpc,
    dir: PathBuf,
    fired: PathBuf,
    command: String,
    /// A second marker, for control binds whose firing must not be confused with the home bind's.
    control_fired: PathBuf,
    control_command: String,
    /// Reload the config on drop: the test defined a submap, which `hl.unbind` can't remove.
    reload_on_drop: bool,
}

impl Fixture {
    fn new() -> Fixture {
        assert!(nested());
        let lock = serialize();
        let ipc = HyprIpc::from_env().unwrap();
        let dir = std::env::temp_dir().join(format!("crosspane home bind {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fire.sh");
        std::fs::write(&script, "#!/bin/sh\necho x >> \"$1\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fired = dir.join("fired");
        let command = format!("'{}' '{}'", script.display(), fired.display());
        let control_fired = dir.join("control-fired");
        let control_command = format!("'{}' '{}'", script.display(), control_fired.display());
        let f = Fixture {
            _lock: lock,
            ipc,
            dir,
            fired,
            command,
            control_fired,
            control_command,
            reload_on_drop: false,
        };
        // Start from a clean nest even if an earlier run died holding binds.
        f.cleanup();
        assert!(
            f.on_chord().is_empty(),
            "the nest still has binds on the chord"
        );
        f
    }

    fn bind(&self) -> HomeBind {
        HomeBind::new(self.ipc.clone(), &default_chord(), &self.command).unwrap()
    }

    fn binds(&self) -> Vec<Value> {
        self.ipc.json("binds").unwrap().as_array().unwrap().clone()
    }

    /// Every bind on the home bind's chord (modmask 13, key Escape in any case).
    fn on_chord(&self) -> Vec<Value> {
        self.binds()
            .into_iter()
            .filter(|b| {
                b["modmask"] == CHORD_MODMASK
                    && b["key"]
                        .as_str()
                        .is_some_and(|key| key.eq_ignore_ascii_case("Escape"))
            })
            .collect()
    }

    fn descriptions(&self) -> Vec<String> {
        self.binds()
            .iter()
            .map(|b| b["description"].as_str().unwrap().to_owned())
            .collect()
    }

    /// An owner's bind, bound straight through Lua (not through `HomeBind`).
    fn owner_bind(&self, keys: &str, description: &str, options: &str) {
        let lua = format!(
            "hl.bind(\"{keys}\", hl.dsp.exec_cmd([==[{}]==]), {{ description = \"{description}\"{options} }})",
            self.command
        );
        self.ipc.eval(&lua).unwrap();
    }

    /// An ordinary owner's bind on `CONTROL_KEYS` (no `dont_inhibit`, default submap) whose command
    /// leaves its mark in the control marker, not the home bind's.
    fn control_bind(&self) {
        let lua = format!(
            "hl.bind(\"{CONTROL_KEYS}\", hl.dsp.exec_cmd([==[{}]==]), {{ description = \"plain control\" }})",
            self.control_command
        );
        self.ipc.eval(&lua).unwrap();
    }

    fn control_count(&self) -> usize {
        std::fs::read_to_string(&self.control_fired).map_or(0, |text| text.lines().count())
    }

    fn wait_control(&self, at_least: usize) {
        wait_until("the control bind's command to run", FIRE_TIMEOUT, || {
            self.control_count() >= at_least
        });
    }

    /// Wait long enough for a control bind that should not fire to have fired, then check.
    fn assert_control_still(&self, count: usize, why: &str) {
        std::thread::sleep(QUIET);
        assert_eq!(self.control_count(), count, "{why}");
    }

    fn fired(&self) -> usize {
        std::fs::read_to_string(&self.fired).map_or(0, |text| text.lines().count())
    }

    fn wait_fired(&self, at_least: usize) {
        wait_until("the bind's command to run", FIRE_TIMEOUT, || {
            self.fired() >= at_least
        });
    }

    /// Wait long enough for a bind that should not fire to have fired, then check the count.
    fn assert_still(&self, count: usize, why: &str) {
        std::thread::sleep(QUIET);
        assert_eq!(self.fired(), count, "{why}");
    }

    fn cleanup(&self) {
        // Leave a submap before its binds go; unbinding an absent chord is harmless.
        let _ = self.ipc.dispatch("hl.dsp.submap(\"reset\")");
        for keys in [
            CHORD_KEYS,
            CONTROL_KEYS,
            "CTRL + SHIFT + ALT + F9",
            "CTRL + ALT + F9",
        ] {
            let _ = self.ipc.eval(&format!("hl.unbind(\"{keys}\")"));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.cleanup();
        if self.reload_on_drop {
            let _ = self.ipc.request("reload");
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---- Wayland clients -----------------------------------------------------------------------

#[derive(Default)]
struct State {
    sync: u64,
    configured: bool,
    keyboard_focus: bool,
    inhibitor_active: Option<bool>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
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
impl Dispatch<wl_callback::WlCallback, u64> for State {
    fn event(
        s: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        token: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.sync = *token;
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { .. } => s.keyboard_focus = true,
            wl_keyboard::Event::Leave { .. } => s.keyboard_focus = false,
            _ => (),
        }
    }
}
impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}
impl Dispatch<XdgSurface, ()> for State {
    fn event(
        s: &mut Self,
        surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            s.configured = true;
        }
    }
}
impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitorV1,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => s.inhibitor_active = Some(true),
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => {
                s.inhibitor_active = Some(false);
            }
            _ => (),
        }
    }
}
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore XdgToplevel);
delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);

/// A connection to the nest with the helpers both clients share.
struct Client {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    qh: QueueHandle<State>,
    token: u64,
}

impl Client {
    fn connect() -> (Client, wayland_client::globals::GlobalList) {
        // Before any connection: `connect_to_env` would honour an inherited WAYLAND_SOCKET.
        require_nest();
        let conn = Connection::connect_to_env().unwrap();
        let (globals, queue) = registry_queue_init::<State>(&conn).unwrap();
        let qh = queue.handle();
        (
            Client {
                conn,
                queue,
                state: State::default(),
                qh,
                token: 0,
            },
            globals,
        )
    }

    fn pump(&mut self) {
        self.queue.dispatch_pending(&mut self.state).unwrap();
        self.conn.flush().unwrap();
        if let Some(guard) = self.conn.prepare_read() {
            let mut fds = [PollFd::new(&self.conn, PollFlags::IN)];
            let timeout = Timespec::try_from(Duration::from_millis(1)).unwrap();
            if poll(&mut fds, Some(&timeout)).unwrap() > 0 {
                guard.read().unwrap();
            }
        }
        self.queue.dispatch_pending(&mut self.state).unwrap();
    }

    /// A round trip: every request sent so far has been handled by the compositor.
    fn sync(&mut self) {
        self.token += 1;
        self.conn.display().sync(&self.qh, self.token);
        let token = self.token;
        self.wait("a round trip", |s| s.sync >= token);
    }

    fn wait(&mut self, what: &str, done: impl Fn(&State) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(&self.state) {
            self.pump();
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
        }
    }
}

fn now_ms() -> u32 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    (time.tv_sec as u64 * 1000 + time.tv_nsec as u64 / 1_000_000) as u32
}

/// A virtual keyboard: the stand-in for physical input (and for Crosspane's own injector, which
/// Hyprland can't tell from a physical keyboard, F2).
struct Keyboard {
    client: Client,
    keyboard: ZwpVirtualKeyboardV1,
    depressed: u32,
}

impl Keyboard {
    fn new() -> Keyboard {
        let (mut client, globals) = Client::connect();
        let seat: wl_seat::WlSeat = globals.bind(&client.qh, 1..=9, ()).unwrap();
        let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&client.qh, 1..=1, ()).unwrap();
        let keyboard = manager.create_virtual_keyboard(&seat, &client.qh, ());
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let mut text = keymap
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
            .into_bytes();
        text.push(0);
        let mut file = File::from(
            rustix::fs::memfd_create("home-bind-test-keymap", rustix::fs::MemfdFlags::CLOEXEC)
                .unwrap(),
        );
        file.write_all(&text).unwrap();
        keyboard.keymap(1, file.as_fd(), text.len() as u32);
        client.sync();
        Keyboard {
            client,
            keyboard,
            depressed: 0,
        }
    }

    fn key(&mut self, code: u32, down: bool) {
        let mask = match code {
            KEY_LEFTSHIFT => 1,
            KEY_LEFTCTRL => 4,
            KEY_LEFTALT => 8,
            _ => 0,
        };
        self.keyboard.key(now_ms(), code, u32::from(down));
        if mask != 0 {
            if down {
                self.depressed |= mask;
            } else {
                self.depressed &= !mask;
            }
            self.keyboard.modifiers(self.depressed, 0, 0, 0);
        }
        self.client.sync();
    }

    fn chord_down(&mut self, modifiers: &[u32], key: u32) {
        for m in modifiers {
            self.key(*m, true);
        }
        self.key(key, true);
    }

    fn chord_up(&mut self, modifiers: &[u32], key: u32) {
        self.key(key, false);
        for m in modifiers.iter().rev() {
            self.key(*m, false);
        }
    }

    fn press(&mut self, modifiers: &[u32], key: u32) {
        self.chord_down(modifiers, key);
        self.chord_up(modifiers, key);
    }
}

/// A toplevel that holds the keyboard focus and can take and drop a shortcuts inhibitor, like a
/// remote desktop or VM window (or W itself) does.
struct Window {
    client: Client,
    surface: wl_surface::WlSurface,
    seat: wl_seat::WlSeat,
    manager: ZwpKeyboardShortcutsInhibitManagerV1,
    _xdg: XdgSurface,
    _toplevel: XdgToplevel,
    _buffer: wl_buffer::WlBuffer,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
}

impl Window {
    /// A mapped window that has the keyboard focus and holds no inhibitor yet.
    fn new() -> Window {
        let (mut client, globals) = Client::connect();
        let qh = client.qh.clone();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let base: XdgWmBase = globals.bind(&qh, 1..=6, ()).unwrap();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).unwrap();
        let manager: ZwpKeyboardShortcutsInhibitManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
        let _keyboard = seat.get_keyboard(&qh, ());
        let surface = compositor.create_surface(&qh, ());
        let xdg = base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_title("crosspane-home-bind-inhibitor".into());
        surface.commit();
        client.wait("the toplevel's configure", |s| s.configured);
        let (width, height) = (64_i32, 64_i32);
        let file = File::from(
            rustix::fs::memfd_create("home-bind-test-window", rustix::fs::MemfdFlags::CLOEXEC)
                .unwrap(),
        );
        file.set_len(width as u64 * height as u64 * 4).unwrap();
        let pool = shm.create_pool(file.as_fd(), width * height * 4, &qh, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            &qh,
            (),
        );
        pool.destroy();
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        client.wait("the window's keyboard focus", |s| s.keyboard_focus);
        Window {
            client,
            surface,
            seat,
            manager,
            _xdg: xdg,
            _toplevel: toplevel,
            _buffer: buffer,
            inhibitor: None,
        }
    }

    /// Ask the compositor to inhibit shortcuts while this window has the focus, and wait until it
    /// says the inhibitor is active.
    fn inhibit(&mut self) {
        assert!(self.inhibitor.is_none(), "already inhibiting");
        self.client.state.inhibitor_active = None;
        self.inhibitor =
            Some(
                self.manager
                    .inhibit_shortcuts(&self.surface, &self.seat, &self.client.qh, ()),
            );
        self.client
            .wait("the shortcuts inhibitor to become active", |s| {
                s.inhibitor_active == Some(true)
            });
    }

    /// Destroy the inhibitor; the window keeps the focus. Returns once the compositor has handled
    /// the request.
    fn release_inhibitor(&mut self) {
        let Some(inhibitor) = self.inhibitor.take() else {
            panic!("no inhibitor to release");
        };
        inhibitor.destroy();
        self.client.state.inhibitor_active = None;
        self.client.sync();
    }
}

// ---- tests ---------------------------------------------------------------------------------

#[test]
fn bind_installs_and_lists() {
    if !nested() {
        return;
    }
    let f = Fixture::new();
    let bind = f.bind();
    assert_eq!(bind.keys(), CHORD_KEYS);
    assert!(!bind.installed().unwrap());

    bind.install().unwrap();
    assert!(bind.installed().unwrap());
    // P8 and U9: `hl.bind` through the IPC `eval` takes effect at run time, and `binds -j` lists
    // `submap_universal` as the string "true".
    let on = f.on_chord();
    eprintln!("binds -j on the chord: {}", Value::Array(on.clone()));
    assert_eq!(on.len(), 1, "{on:?}");
    let listed = &on[0];
    assert_eq!(listed["description"], DESCRIPTION);
    assert_eq!(listed["key"], "Escape");
    assert_eq!(listed["modmask"], CHORD_MODMASK);
    assert_eq!(listed["submap_universal"], "true");
    assert_eq!(listed["submap"], "");
    assert_eq!(listed["release"], false);
    assert_eq!(listed["repeat"], false);
    assert_eq!(listed["dispatcher"], "__lua");
    // The nest's own binds are untouched.
    assert!(
        f.descriptions()
            .contains(&"Exit Crosspane test session".into())
    );

    // Installing again leaves exactly one.
    bind.install().unwrap();
    assert_eq!(f.on_chord().len(), 1);
    assert!(bind.installed().unwrap());

    bind.remove().unwrap();
    assert!(f.on_chord().is_empty());
    assert!(!bind.installed().unwrap());
    assert!(
        f.descriptions()
            .contains(&"Exit Crosspane test session".into())
    );
}

#[test]
fn virtual_chord_fires_bind() {
    if !nested() {
        return;
    }
    let f = Fixture::new();
    let bind = f.bind();
    bind.install().unwrap();
    let mut kb = Keyboard::new();

    // F2: a virtual keyboard presses a keybind exactly like a physical one. This is why the
    // provenance invariant (no Crosspane injection while the bind exists) lives in the engine.
    kb.press(&CHORD_MODS, KEY_ESC);
    f.wait_fired(1);
    assert_eq!(f.fired(), 1);

    // Controls: the marker discriminates. A chord missing a modifier, and the same keys with an
    // extra modifier, don't fire it (Hyprland matches modifier masks exactly).
    kb.press(&[KEY_LEFTSHIFT, KEY_LEFTALT], KEY_ESC);
    f.assert_still(1, "SHIFT + ALT + Escape must not fire the bind");
    kb.press(&[KEY_LEFTCTRL, KEY_LEFTSHIFT], KEY_ESC);
    f.assert_still(1, "CTRL + SHIFT + Escape must not fire the bind");
    kb.press(&[KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_LEFTALT], KEY_F11);
    f.assert_still(1, "another key must not fire the bind");

    // Pressed again, it fires again; once removed, it doesn't.
    kb.press(&CHORD_MODS, KEY_ESC);
    f.wait_fired(2);
    bind.remove().unwrap();
    kb.press(&CHORD_MODS, KEY_ESC);
    f.assert_still(2, "a removed bind must not fire");
}

#[test]
fn press_only_no_release_bind() {
    if !nested() {
        return;
    }
    let f = Fixture::new();
    f.bind().install().unwrap();
    let mut kb = Keyboard::new();

    kb.chord_down(&CHORD_MODS, KEY_ESC);
    f.wait_fired(1);
    // Holding: one fire for the press, however long it is held.
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(f.fired(), 1, "holding the chord fired more than once");
    // Releasing: nothing fires.
    kb.chord_up(&CHORD_MODS, KEY_ESC);
    f.assert_still(1, "releasing the chord fired the bind");
    // There is no second (release) bind on the chord.
    assert_eq!(f.on_chord().len(), 1);
    // A fresh press is a fresh fire.
    kb.press(&CHORD_MODS, KEY_ESC);
    f.wait_fired(2);
}

#[test]
fn fires_in_other_submap() {
    if !nested() {
        return;
    }
    let mut f = Fixture::new();
    f.reload_on_drop = true;
    f.bind().install().unwrap();
    // A submap exists once it has a bind. The control bind is a plain bind in the default submap
    // on another chord; it must stop firing in the other submap while ours keeps firing.
    f.ipc
        .eval(
            "hl.define_submap(\"crosspane-test-other\", function() \
             hl.bind(\"CTRL + SHIFT + ALT + F9\", hl.dsp.exec_cmd(\"true\"), { description = \"in-other\" }) \
             end)",
        )
        .unwrap();
    f.control_bind();
    let mut kb = Keyboard::new();
    // Baseline: in the default submap both the control and the home bind fire.
    kb.press(&CHORD_MODS, KEY_F11);
    f.wait_control(1);
    kb.press(&CHORD_MODS, KEY_ESC);
    f.wait_fired(1);

    f.ipc
        .dispatch("hl.dsp.submap(\"crosspane-test-other\")")
        .unwrap();
    assert_eq!(
        f.ipc.request("submap").unwrap().trim(),
        "crosspane-test-other"
    );
    kb.press(&CHORD_MODS, KEY_F11);
    f.assert_control_still(
        1,
        "the plain bind fired in another submap: the control has no teeth",
    );
    kb.press(&CHORD_MODS, KEY_ESC);
    f.wait_fired(2);
    assert_eq!(f.fired(), 2);

    // Back in the default submap the control fires again.
    f.ipc.dispatch("hl.dsp.submap(\"reset\")").unwrap();
    kb.press(&CHORD_MODS, KEY_F11);
    f.wait_control(2);
    kb.press(&CHORD_MODS, KEY_ESC);
    f.wait_fired(3);
}

#[test]
fn fires_under_shortcuts_inhibitor() {
    if !nested() {
        return;
    }
    let f = Fixture::new();
    f.bind().install().unwrap();
    f.control_bind();
    let mut kb = Keyboard::new();
    // A window that has the keyboard focus, holding no inhibitor yet.
    let mut window = Window::new();

    // Baseline: with the focused window and no inhibitor, the ordinary control bind works. Without
    // this, "suppressed" below could just mean "never fires".
    kb.press(&CHORD_MODS, KEY_F11);
    f.wait_control(1);
    f.assert_control_still(1, "one press of the control bind fired it more than once");
    assert_eq!(f.fired(), 0, "the home bind fired on the control chord");

    // The window takes the inhibitor: the ordinary bind is suppressed, the home bind
    // (`dont_inhibit`) still fires, so no window can trap the user.
    window.inhibit();
    kb.press(&CHORD_MODS, KEY_F11);
    f.assert_control_still(
        1,
        "an ordinary bind fired under an inhibitor: the control has no teeth",
    );
    kb.press(&CHORD_MODS, KEY_ESC);
    f.wait_fired(1);
    assert_eq!(f.fired(), 1);
    assert_eq!(
        f.control_count(),
        1,
        "the control bind fired on the home chord"
    );
    window.client.pump();
    assert_eq!(window.client.state.inhibitor_active, Some(true));

    // The inhibitor goes: the ordinary bind fires again, so the suppression was the inhibitor's.
    window.release_inhibitor();
    kb.press(&CHORD_MODS, KEY_F11);
    f.wait_control(2);
    f.assert_control_still(2, "one press of the control bind fired it more than once");
    // And the home bind fires as before.
    kb.press(&CHORD_MODS, KEY_ESC);
    f.wait_fired(2);
}

#[test]
fn remove_is_idempotent() {
    if !nested() {
        return;
    }
    let f = Fixture::new();
    let bind = f.bind();
    // Nothing installed: a no-op.
    bind.remove().unwrap();
    bind.install().unwrap();
    bind.remove().unwrap();
    bind.remove().unwrap();
    assert!(f.on_chord().is_empty());
    assert!(!bind.installed().unwrap());
    // The nest's own bind (CTRL + ALT + Escape) was never touched.
    assert!(
        f.descriptions()
            .contains(&"Exit Crosspane test session".into())
    );
}

#[test]
fn install_refuses_collision() {
    if !nested() {
        return;
    }
    let f = Fixture::new();
    let bind = f.bind();
    f.owner_bind(CHORD_KEYS, "owner's bind", "");
    let before = f.on_chord();
    assert_eq!(before.len(), 1);

    let err = bind.install().unwrap_err();
    assert!(matches!(err, PlatformError::Backend(_)), "{err}");
    // The owner's bind is untouched, and nothing of ours was added.
    assert_eq!(f.on_chord(), before);
    assert!(!bind.installed().unwrap());

    // Another spelling of the same chord collides too (`hl.unbind` would not remove it, but two
    // binds on one chord would both fire).
    f.ipc.eval(&format!("hl.unbind(\"{CHORD_KEYS}\")")).unwrap();
    f.owner_bind("CTRL + SHIFT + ALT + escape", "lower-case spelling", "");
    assert!(bind.install().is_err());
    assert_eq!(f.on_chord().len(), 1);
    assert_eq!(f.on_chord()[0]["description"], "lower-case spelling");
}

#[test]
fn startup_cleanup_leaves_a_foreign_bind_intact() {
    if !nested() {
        return;
    }
    // B5: only a foreign bind on the chord means ours is absent. `remove` (the agent's startup and
    // shutdown cleanup) reports `Ok` and does not unbind it.
    let f = Fixture::new();
    let bind = f.bind();
    f.owner_bind(CHORD_KEYS, "owner's bind", "");
    let before = f.on_chord();
    bind.remove().unwrap();
    bind.remove().unwrap();
    assert_eq!(f.on_chord(), before);
    assert!(!bind.installed().unwrap());
    // The owner's bind still works.
    let mut kb = Keyboard::new();
    kb.press(&CHORD_MODS, KEY_ESC);
    f.wait_fired(1);
}

#[test]
fn shutdown_with_ours_present_removes_only_ours() {
    if !nested() {
        return;
    }
    let f = Fixture::new();
    let bind = f.bind();
    // The owner's neighbours on other chords survive.
    f.owner_bind("CTRL + SHIFT + ALT + F9", "owner's neighbour", "");
    f.owner_bind("CTRL + ALT + F9", "owner's other neighbour", "");
    bind.install().unwrap();
    bind.remove().unwrap();
    assert!(f.on_chord().is_empty());
    let descriptions = f.descriptions();
    assert!(
        descriptions.contains(&"owner's neighbour".into()),
        "{descriptions:?}"
    );
    assert!(
        descriptions.contains(&"owner's other neighbour".into()),
        "{descriptions:?}"
    );
}

#[test]
fn foreign_bind_appearing_while_home_is_not_removed() {
    if !nested() {
        return;
    }
    // A2: ours and a foreign bind on the chord. `hl.unbind` would take both, so `remove` refuses
    // (`Err`, the caller keeps its teardown fence) and unbinds nothing.
    let f = Fixture::new();
    let bind = f.bind();
    bind.install().unwrap();
    f.owner_bind(CHORD_KEYS, "owner's late bind", "");
    assert_eq!(f.on_chord().len(), 2);
    assert!(
        !bind.installed().unwrap(),
        "the chord is shared: not exactly ours"
    );

    let err = bind.remove().unwrap_err();
    assert!(matches!(err, PlatformError::Backend(_)), "{err}");
    let on = f.on_chord();
    assert_eq!(on.len(), 2, "{on:?}");
    assert!(on.iter().any(|b| b["description"] == "owner's late bind"));
    assert!(on.iter().any(|b| b["description"] == DESCRIPTION));
    // And install refuses too.
    assert!(bind.install().is_err());

    // A config reload drops runtime binds: the next retry finds the chord clean, and the fence
    // clears.
    f.ipc.request("reload").unwrap();
    wait_until("the reload to clear the binds", FIRE_TIMEOUT, || {
        f.on_chord().is_empty()
    });
    bind.remove().unwrap();
}

#[test]
fn unbind_removes_owner_binds_too() {
    if !nested() {
        return;
    }
    // U7: the premise of the ownership rules. `hl.unbind` removes every bind with the same keys,
    // owner's included, which is why the module reads `binds -j` before it unbinds.
    let f = Fixture::new();
    f.owner_bind(CHORD_KEYS, "owner's bind", "");
    assert_eq!(f.on_chord().len(), 1);
    f.ipc.eval(&format!("hl.unbind(\"{CHORD_KEYS}\")")).unwrap();
    assert!(f.on_chord().is_empty(), "hl.unbind left the owner's bind");
}

#[test]
fn install_replaces_a_stale_copy() {
    if !nested() {
        return;
    }
    let f = Fixture::new();
    let bind = f.bind();
    // A stale copy of ours in the wrong shape (a leftover from some other build).
    f.owner_bind(CHORD_KEYS, DESCRIPTION, "");
    assert_eq!(f.on_chord()[0]["submap_universal"], "false");
    assert!(!bind.installed().unwrap());
    bind.install().unwrap();
    let on = f.on_chord();
    assert_eq!(on.len(), 1, "{on:?}");
    assert_eq!(on[0]["submap_universal"], "true");
    assert!(bind.installed().unwrap());
}

#[test]
fn reload_clears_bind() {
    if !nested() {
        return;
    }
    let f = Fixture::new();
    let bind = f.bind();
    bind.install().unwrap();
    assert!(bind.installed().unwrap());

    // A config reload drops every runtime bind and keeps the config's own.
    f.ipc.request("reload").unwrap();
    wait_until("the reload to clear the bind", FIRE_TIMEOUT, || {
        !bind.installed().unwrap()
    });
    assert!(
        f.descriptions()
            .contains(&"Exit Crosspane test session".into())
    );

    // The agent's re-verification then reinstalls it.
    bind.install().unwrap();
    assert!(bind.installed().unwrap());
    assert_eq!(f.on_chord().len(), 1);
}

#[test]
fn long_command_is_not_truncated() {
    if !nested() {
        return;
    }
    let f = Fixture::new();
    // The command rides in one IPC request line; a long one must arrive whole and still run.
    let command = format!("{} # {}", f.command, "x".repeat(3000));
    let bind = HomeBind::new(f.ipc.clone(), &default_chord(), &command).unwrap();
    bind.install().unwrap();
    assert!(bind.installed().unwrap());
    let mut kb = Keyboard::new();
    kb.press(&CHORD_MODS, KEY_ESC);
    f.wait_fired(1);
}

/// A self-deleting stand-in for `$XDG_RUNTIME_DIR`.
struct FakeRuntime(PathBuf);

impl FakeRuntime {
    fn new() -> FakeRuntime {
        let dir = std::env::temp_dir().join(format!("cp-nest-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        FakeRuntime(dir)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn endpoints(&self, display: Option<&str>, signature: Option<&str>) -> Endpoints {
        Endpoints {
            runtime_dir: self.0.clone(),
            wayland_socket: None,
            wayland_display: display.map(str::to_owned),
            signature: signature.map(str::to_owned),
        }
    }
}

impl Drop for FakeRuntime {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn nest_guard_refuses_inherited_sockets_and_foreign_endpoints() {
    // No compositor needed: the guard only reads files. The pid of this test process stands in for
    // a running compositor.
    let rt = FakeRuntime::new();
    let pid = std::process::id().to_string();
    let nest_env = |sig: &str, display: &str| {
        format!(
            "unset WAYLAND_SOCKET\nexport HYPRLAND_INSTANCE_SIGNATURE={sig}\n\
             export WAYLAND_DISPLAY={display}\nexport CROSSPANE_NESTED_HYPR=1\n"
        )
    };
    rt.write("hypr/sigNEST/hyprland.lock", &format!("{pid}\nwayland-9\n"));
    rt.write("crosspane-hypr-t/env", &nest_env("sigNEST", "wayland-9"));
    rt.write("crosspane-hypr-t/pid", &format!("{pid}\n"));
    // The "live session": an instance with its own lock and no nest state directory.
    rt.write("hypr/sigLIVE/hyprland.lock", "1\nwayland-1\n");

    // The nest's own endpoints pass.
    assert_eq!(
        verify_nest(&rt.endpoints(Some("wayland-9"), Some("sigNEST"))),
        Ok(())
    );

    // An inherited WAYLAND_SOCKET is refused even when everything else is the nest's.
    let mut inherited = rt.endpoints(Some("wayland-9"), Some("sigNEST"));
    inherited.wayland_socket = Some("7".into());
    let err = verify_nest(&inherited).unwrap_err();
    assert!(err.contains("WAYLAND_SOCKET"), "{err}");
    inherited.wayland_socket = Some("".into());
    assert!(verify_nest(&inherited).is_err(), "an empty one counts too");

    // Missing or implausible endpoints.
    for (display, signature) in [
        (None, Some("sigNEST")),
        (Some(""), Some("sigNEST")),
        (Some("wayland-9"), None),
        (Some("wayland-9"), Some("")),
        (Some("wayland-9"), Some("../sigNEST")),
        (Some("wayland-9"), Some("sig/NEST")),
        (Some("wayland-9"), Some("sigMISSING")),
    ] {
        assert!(
            verify_nest(&rt.endpoints(display, signature)).is_err(),
            "{display:?} {signature:?}"
        );
    }

    // The live session: its signature (even with its own socket name) is not a nest, and the
    // nest's signature does not pair with the live socket.
    for (display, signature) in [
        ("wayland-1", "sigLIVE"),
        ("wayland-9", "sigLIVE"),
        ("wayland-1", "sigNEST"),
        ("wayland-0", "sigNEST"),
    ] {
        assert!(
            verify_nest(&rt.endpoints(Some(display), Some(signature))).is_err(),
            "{display} {signature}"
        );
    }

    // The nest's state must agree with Hyprland's lock: same compositor pid, still running, same
    // exports.
    let good = rt.endpoints(Some("wayland-9"), Some("sigNEST"));
    rt.write("crosspane-hypr-t/pid", "999999998\n");
    assert!(verify_nest(&good).is_err(), "the state names another pid");
    rt.write("crosspane-hypr-t/pid", &format!("{pid}\n"));
    rt.write("crosspane-hypr-t/env", &nest_env("sigNEST", "wayland-8"));
    assert!(
        verify_nest(&good).is_err(),
        "the state names another socket"
    );
    rt.write("crosspane-hypr-t/env", &nest_env("sigOTHER", "wayland-9"));
    assert!(
        verify_nest(&good).is_err(),
        "the state names another instance"
    );
    rt.write("crosspane-hypr-t/env", &nest_env("sigNEST", "wayland-9"));
    assert_eq!(verify_nest(&good), Ok(()));
    // A dead compositor (the pid is gone) is not a nest to talk to.
    rt.write("hypr/sigDEAD/hyprland.lock", "999999999\nwayland-7\n");
    rt.write("crosspane-hypr-d/env", &nest_env("sigDEAD", "wayland-7"));
    rt.write("crosspane-hypr-d/pid", "999999999\n");
    assert!(verify_nest(&rt.endpoints(Some("wayland-7"), Some("sigDEAD"))).is_err());
    // Only `crosspane-hypr-*` directories count as nest state.
    std::fs::rename(rt.0.join("crosspane-hypr-t"), rt.0.join("other-t")).unwrap();
    assert!(verify_nest(&good).is_err(), "not a nest state directory");
}

#[test]
fn unsupported_key_refused() {
    // No compositor needed: the chord is checked before any request.
    let ipc = HyprIpc::new(
        "no-such-instance",
        Path::new("/nonexistent-runtime-dir"),
        Duration::from_millis(100),
    );
    for refused in [
        // An arrow key, CapsLock, a modifier as the key, a non-modifier as a modifier.
        chord(&[0xE0], 0x4F),
        chord(&[0xE0], 0x39),
        chord(&[0xE0], 0xE1),
        chord(&[0x04], 0x29),
        // Another usage page.
        Chord {
            modifiers: vec![HidUsage::keyboard(0xE0)],
            key: HidUsage {
                page: 0x0C,
                id: 0x29,
            },
        },
    ] {
        let err = HomeBind::new(ipc.clone(), &refused, "true").unwrap_err();
        assert!(
            matches!(err, PlatformError::Unsupported(_)),
            "{refused:?}: {err}"
        );
    }
    // Mapped keys are accepted, left and right modifiers alike.
    for (modifiers, key, keys) in [
        (vec![0xE0, 0xE1, 0xE2], 0x29, "CTRL + SHIFT + ALT + Escape"),
        (vec![0xE4, 0xE7], 0x3A, "CTRL + SUPER + F1"),
        (vec![0xE5], 0x4B, "SHIFT + Page_Up"),
        (vec![0xE6], 0x04, "ALT + a"),
    ] {
        let bind = HomeBind::new(ipc.clone(), &chord(&modifiers, key), "true").unwrap();
        assert_eq!(bind.keys(), keys);
    }
}

#[test]
fn quoting_refused() {
    let ipc = HyprIpc::new(
        "no-such-instance",
        Path::new("/nonexistent-runtime-dir"),
        Duration::from_millis(100),
    );
    for refused in [
        // A `'` in a path that would unbalance the shell quoting.
        "env CROSSPANE_RUNTIME_DIR='/run/user/it's' '/bin/crosspanectl' release",
        "echo it's",
        // The Lua long-string terminator.
        "true ]==] os.execute('x')",
        // ...or one byte early, once the closer is appended.
        "true ]==",
        // Control characters (a newline would end the IPC request line).
        "true\nfalse",
        "true\0",
        // Nothing to run.
        "",
    ] {
        let err = HomeBind::new(ipc.clone(), &default_chord(), refused).unwrap_err();
        assert!(
            matches!(err, PlatformError::Unsupported(_)),
            "{refused:?}: {err}"
        );
    }
    // The command the agent passes is quoted by the agent and is accepted as it is.
    let ok = "env CROSSPANE_RUNTIME_DIR='/run/user/1000/crosspane' \
              '/home/u/.local/bin/crosspanectl' release";
    assert!(HomeBind::new(ipc, &default_chord(), ok).is_ok());
}
