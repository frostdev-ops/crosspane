//! C2a tests use the production module directly for its private pure/pipe seam and marker.
//! The only native source below is a test fixture on the explicitly named, owned C2 nest.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

#[path = "../src/hyprland/clipboard.rs"]
mod implementation;

use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use crosspane_platform::{ClipKinds, ClipboardEvent, ClipboardHost, IoGate, PlatformError};
use crosspane_types::ClipKind;
use implementation::{HyprlandClipboard, connection_guard, read};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::protocol::{wl_callback, wl_registry, wl_seat};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child,
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

fn open_gate() -> Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}

fn types(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).into()).collect()
}

#[test]
fn mime_mapping_and_text_preference_order() {
    assert_eq!(read::kinds(&[]), ClipKinds::default());
    for (index, mime) in read::TEXT.iter().enumerate() {
        let offered = types(&read::TEXT[index..]);
        assert_eq!(read::mime(&offered, ClipKind::Text), Some(*mime));
        assert_eq!(
            read::kinds(&offered),
            ClipKinds {
                text: true,
                image: false
            }
        );
    }
    let offered = types(&["image/png", "unrecognized/type"]);
    assert_eq!(read::mime(&offered, ClipKind::Image), Some("image/png"));
    assert_eq!(read::mime(&offered, ClipKind::Text), None);
    assert_eq!(
        read::kinds(&offered),
        ClipKinds {
            text: false,
            image: true
        }
    );
    assert_eq!(
        read::kinds(&types(&["STRING", "image/png"])),
        ClipKinds {
            text: true,
            image: true
        }
    );
}

#[test]
fn marker_recognition_requires_this_exact_instance() {
    let marker = "application/x-crosspane-promise-0123456789abcdef0123456789abcdef";
    assert!(read::own(&types(&["text/plain", marker]), marker));
    assert!(!read::own(
        &types(&["application/x-crosspane-promise-ffffffffffffffffffffffffffffffff"]),
        marker
    ));
    assert!(!read::own(&types(&["text/plain"]), marker));
}

fn pipe_read(data: &[u8], kind: ClipKind, max: usize) -> Result<Vec<u8>, PlatformError> {
    let (reader, mut writer) = std::io::pipe().unwrap();
    read::nonblocking(&reader).unwrap();
    writer.write_all(data).unwrap();
    drop(writer);
    let gate = open_gate();
    read::receive(
        reader,
        kind,
        max,
        Instant::now() + Duration::from_secs(1),
        &gate,
        gate.epoch(),
    )
}

#[test]
fn read_limit_rejects_max_plus_one_without_truncation() {
    assert!(pipe_read(b"abcd", ClipKind::Text, 4).unwrap() == b"abcd");
    assert!(matches!(
        pipe_read(b"abcde", ClipKind::Text, 4),
        Err(PlatformError::TooLarge)
    ));
    assert!(matches!(
        pipe_read(b"x", ClipKind::Text, 0),
        Err(PlatformError::TooLarge)
    ));
    assert!(pipe_read(b"", ClipKind::Text, 0).unwrap().is_empty());
}

#[test]
fn invalid_utf8_is_not_found_including_legacy_string() {
    assert_eq!(
        read::mime(&types(&["STRING"]), ClipKind::Text),
        Some("STRING")
    );
    assert!(matches!(
        pipe_read(&[0xff], ClipKind::Text, 2),
        Err(PlatformError::NotFound)
    ));
    assert!(pipe_read(&[0xff], ClipKind::Image, 2).unwrap() == [0xff]);
}

#[test]
fn bounded_wait_expires_and_closes_the_reader() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    read::nonblocking(&reader).unwrap();
    let gate = open_gate();
    let start = Instant::now();
    assert!(matches!(
        read::receive(
            reader,
            ClipKind::Text,
            10,
            start + Duration::from_millis(30),
            &gate,
            gate.epoch()
        ),
        Err(PlatformError::Timeout)
    ));
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(writer.write_all(b"x").is_err());
}

#[test]
fn closed_or_changed_gate_cancels_a_read_and_closes_the_reader() {
    for reopen in [false, true] {
        let (reader, mut writer) = std::io::pipe().unwrap();
        read::nonblocking(&reader).unwrap();
        let gate = open_gate();
        let epoch = gate.epoch();
        gate.set_engine_permits(false);
        if reopen {
            gate.set_engine_permits(true);
        }
        assert!(matches!(
            read::receive(
                reader,
                ClipKind::Text,
                10,
                Instant::now() + Duration::from_secs(1),
                &gate,
                epoch
            ),
            Err(PlatformError::Locked)
        ));
        assert!(writer.write_all(b"x").is_err());
    }
}

#[test]
fn gate_closing_during_a_pending_read_cancels_without_waiting_for_timeout() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    read::nonblocking(&reader).unwrap();
    let gate = open_gate();
    let epoch = gate.epoch();
    let closing = gate.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        closing.set_session_permits(false);
    });
    let start = Instant::now();
    assert!(matches!(
        read::receive(
            reader,
            ClipKind::Text,
            10,
            start + Duration::from_secs(1),
            &gate,
            epoch
        ),
        Err(PlatformError::Locked)
    ));
    thread.join().unwrap();
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(writer.write_all(b"x").is_err());
}

#[derive(Default)]
struct FixtureState {
    globals: Vec<(u32, String, u32)>,
    synced: bool,
    sends: usize,
    held: Vec<OwnedFd>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for FixtureState {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state.globals.push((name, interface, version));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for FixtureState {
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

impl Dispatch<ExtDataControlDeviceV1, ()> for FixtureState {
    fn event(
        _: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::Selection { id: Some(offer) }
            | ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                offer.destroy()
            }
            _ => {}
        }
    }
    event_created_child!(FixtureState, ExtDataControlDeviceV1, [0 => (ExtDataControlOfferV1, ())]);
}

impl Dispatch<ExtDataControlSourceV1, Option<Vec<u8>>> for FixtureState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        data: &Option<Vec<u8>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_source_v1::Event::Send { fd, .. } = event {
            state.sends += 1;
            if let Some(data) = data {
                read::nonblocking(&fd).unwrap();
                File::from(fd).write_all(data).unwrap();
            } else {
                state.held.push(fd);
            }
        }
    }
}

delegate_noop!(FixtureState: ignore wl_seat::WlSeat);
delegate_noop!(FixtureState: ignore ExtDataControlManagerV1);
delegate_noop!(FixtureState: ignore ExtDataControlOfferV1);

fn fixture_pump(
    connection: &Connection,
    queue: &mut EventQueue<FixtureState>,
    state: &mut FixtureState,
) {
    queue.dispatch_pending(state).unwrap();
    connection.flush().unwrap();
    if let Some(guard) = queue.prepare_read() {
        let fd = guard.connection_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 10_000_000,
        };
        if poll(&mut fds, Some(&timeout)).unwrap() != 0 {
            guard.read().unwrap();
        }
    }
    queue.dispatch_pending(state).unwrap();
}

fn fixture_sync(
    connection: &Connection,
    queue: &mut EventQueue<FixtureState>,
    state: &mut FixtureState,
) {
    state.synced = false;
    connection.display().sync(&queue.handle(), ());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state.synced {
        assert!(Instant::now() < deadline, "fixture sync timed out");
        fixture_pump(connection, queue, state);
    }
}

enum FixtureCommand {
    Publish(Vec<String>, Option<Vec<u8>>, bool, SyncSender<()>),
    Sends(SyncSender<usize>),
}

struct Fixture {
    commands: SyncSender<FixtureCommand>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(display_socket: std::path::PathBuf) -> Self {
        let (commands, input) = mpsc::sync_channel(8);
        let (ready, wait) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let connection = connection_guard::connect(display_socket).unwrap();
            let mut queue = connection.new_event_queue();
            let registry = connection.display().get_registry(&queue.handle(), ());
            let mut state = FixtureState::default();
            fixture_sync(&connection, &mut queue, &mut state);
            let find = |name| {
                state
                    .globals
                    .iter()
                    .find(|(_, interface, version)| interface == name && *version >= 1)
                    .unwrap()
                    .0
            };
            let manager: ExtDataControlManagerV1 = registry.bind(
                find(ExtDataControlManagerV1::interface().name),
                1,
                &queue.handle(),
                (),
            );
            let seat: wl_seat::WlSeat = registry.bind(
                find(wl_seat::WlSeat::interface().name),
                1,
                &queue.handle(),
                (),
            );
            let device = manager.get_data_device(&seat, &queue.handle(), ());
            fixture_sync(&connection, &mut queue, &mut state);
            let mut source: Option<ExtDataControlSourceV1> = None;
            ready.send(()).unwrap();
            while !stopped.load(Ordering::Acquire) {
                fixture_pump(&connection, &mut queue, &mut state);
                match input.try_recv() {
                    Ok(FixtureCommand::Publish(types, data, primary, reply)) => {
                        let previous = source.take();
                        state.held.clear();
                        if !types.is_empty() {
                            let next = manager.create_data_source(&queue.handle(), data);
                            for mime in types {
                                next.offer(mime);
                            }
                            if primary {
                                device.set_primary_selection(Some(&next));
                            } else {
                                device.set_selection(Some(&next));
                            }
                            source = Some(next);
                        } else {
                            device.set_selection(None);
                        }
                        // Replace first, so destroying the old source does not clear the new
                        // selection and create an unrelated Changed before the marker check.
                        if let Some(previous) = previous {
                            previous.destroy();
                        }
                        fixture_sync(&connection, &mut queue, &mut state);
                        reply.send(()).unwrap();
                    }
                    Ok(FixtureCommand::Sends(reply)) => {
                        reply.send(state.sends).unwrap();
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
            if let Some(source) = source {
                source.destroy();
            }
            state.held.clear();
            device.destroy();
            manager.destroy();
            let _ = connection.flush();
        });
        let fixture = Self {
            commands,
            stop,
            thread: Some(thread),
        };
        wait.recv_timeout(Duration::from_secs(3)).unwrap();
        fixture
    }

    fn publish(&self, offered: Vec<String>, data: Option<Vec<u8>>, primary: bool) {
        let (reply, wait) = mpsc::sync_channel(1);
        self.commands
            .send(FixtureCommand::Publish(offered, data, primary, reply))
            .unwrap();
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    fn sends(&self) -> usize {
        let (reply, wait) = mpsc::sync_channel(1);
        self.commands.send(FixtureCommand::Sends(reply)).unwrap();
        wait.recv_timeout(Duration::from_secs(2)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn changed(events: &Receiver<ClipboardEvent>, wanted: ClipKinds) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if event == (ClipboardEvent::Changed { kinds: wanted }) {
            return;
        }
    }
}

fn verified_nested_socket(
    environment: &str,
    runtime: &std::path::Path,
    display: &str,
    signature: &str,
) -> Option<std::path::PathBuf> {
    fn value<'a>(environment: &'a str, prefix: &str) -> Option<&'a str> {
        let mut values = environment
            .lines()
            .filter_map(|line| line.strip_prefix(prefix));
        let value = values.next()?;
        // hypr-nested.sh writes these identifiers with bash %q. Accept only its plain,
        // unescaped basename form, and reject duplicate exports rather than guessing precedence.
        if values.next().is_some()
            || value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return None;
        }
        Some(value)
    }
    let recorded_display = value(environment, "export WAYLAND_DISPLAY=")?;
    let recorded_signature = value(environment, "export HYPRLAND_INSTANCE_SIGNATURE=")?;
    (recorded_display == display
        && recorded_signature == signature
        && value(environment, "export CROSSPANE_NESTED_HYPR=") == Some("1"))
    .then(|| runtime.join(recorded_display))
}

#[test]
fn nested_environment_rejects_display_and_signature_prefix_collisions() {
    let environment = "unset WAYLAND_SOCKET\nexport HYPRLAND_INSTANCE_SIGNATURE=nest_123\nexport WAYLAND_DISPLAY=wayland-10\nexport CROSSPANE_NESTED_HYPR=1\n";
    let runtime = std::path::Path::new("/owned-runtime");
    assert!(verified_nested_socket(environment, runtime, "wayland-1", "nest_123").is_none());
    assert!(verified_nested_socket(environment, runtime, "wayland-10", "nest_12").is_none());
    assert_eq!(
        verified_nested_socket(environment, runtime, "wayland-10", "nest_123"),
        Some(runtime.join("wayland-10"))
    );
}

#[test]
fn nested_environment_rejects_duplicate_missing_and_escaped_exports() {
    let environment = "export HYPRLAND_INSTANCE_SIGNATURE=nest_123\nexport WAYLAND_DISPLAY=wayland-10\nexport CROSSPANE_NESTED_HYPR=1\n";
    let runtime = std::path::Path::new("/owned-runtime");
    for invalid in [
        format!("{environment}export WAYLAND_DISPLAY=wayland-1\n"),
        environment.replace("export WAYLAND_DISPLAY=wayland-10\n", ""),
        environment.replace("wayland-10", "../wayland-1"),
        environment.replace("wayland-10", "'wayland-10'"),
        environment.replace("CROSSPANE_NESTED_HYPR=1", "CROSSPANE_NESTED_HYPR=0"),
    ] {
        assert!(verified_nested_socket(&invalid, runtime, "wayland-10", "nest_123").is_none());
    }
}

#[derive(Clone)]
struct NestedConnectionGuard {
    runtime: std::path::PathBuf,
    proc_root: std::path::PathBuf,
    display: String,
    signature: String,
    pid: u32,
    start: String,
    before: Arc<AtomicUsize>,
    after: Arc<AtomicUsize>,
}

impl NestedConnectionGuard {
    fn new(
        runtime: std::path::PathBuf,
        proc_root: std::path::PathBuf,
        display: String,
        signature: String,
    ) -> Self {
        let state = runtime.join("crosspane-hypr-C2");
        let pid = std::fs::read_to_string(state.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let start = std::fs::read_to_string(state.join("start"))
            .unwrap()
            .trim()
            .to_owned();
        Self {
            runtime,
            proc_root,
            display,
            signature,
            pid,
            start,
            before: Arc::new(AtomicUsize::new(0)),
            after: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn verify(&self) -> Option<std::path::PathBuf> {
        use std::os::unix::fs::FileTypeExt;
        let state = self.runtime.join("crosspane-hypr-C2");
        let environment = std::fs::read_to_string(state.join("env")).ok()?;
        let socket =
            verified_nested_socket(&environment, &self.runtime, &self.display, &self.signature)?;
        let pid = std::fs::read_to_string(state.join("pid")).ok()?;
        let start = std::fs::read_to_string(state.join("start")).ok()?;
        if pid.trim() != self.pid.to_string() || start.trim() != self.start {
            return None;
        }
        let process = self.proc_root.join(self.pid.to_string());
        let stat = std::fs::read_to_string(process.join("stat")).ok()?;
        let (prefix, fields) = stat.rsplit_once(") ")?;
        let fields: Vec<_> = fields.split_whitespace().collect();
        if prefix.split_whitespace().next()? != pid.trim()
            || matches!(*fields.first()?, "Z" | "X" | "x")
            || *fields.get(19)? != self.start
            || std::fs::read_to_string(process.join("comm")).ok()?.trim() != "Hyprland"
        {
            return None;
        }
        let native = self.runtime.join("hypr").join(&self.signature);
        let lock = std::fs::read_to_string(native.join("hyprland.lock")).ok()?;
        if lock.lines().collect::<Vec<_>>() != [pid.trim(), self.display.as_str()]
            || !std::fs::metadata(native.join(".socket.sock"))
                .ok()?
                .file_type()
                .is_socket()
            || !std::fs::metadata(&socket).ok()?.file_type().is_socket()
        {
            return None;
        }
        Some(socket)
    }

    fn check(&self, peer: Option<u32>) -> Result<(), PlatformError> {
        let counter = if peer.is_some() {
            &self.after
        } else {
            &self.before
        };
        counter.fetch_add(1, Ordering::SeqCst);
        if self.verify().is_none() || peer.is_some_and(|pid| pid != self.pid) {
            return Err(PlatformError::Backend(
                "owned C2 nest is no longer verified".into(),
            ));
        }
        Ok(())
    }

    fn install(&self) -> connection_guard::Registration {
        let guard = self.clone();
        connection_guard::install(
            self.runtime.join(&self.display),
            Arc::new(move |peer| guard.check(peer)),
        )
    }

    fn assert_connections(&self, wanted: usize) {
        assert_eq!(self.before.load(Ordering::SeqCst), wanted);
        assert_eq!(self.after.load(Ordering::SeqCst), wanted);
    }
}

struct FakeNest {
    guard: NestedConnectionGuard,
    display_listener: std::os::unix::net::UnixListener,
    _ipc_listener: std::os::unix::net::UnixListener,
}

impl FakeNest {
    fn new(pid: u32) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "crosspane-c2-guard-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        // This entirely fake runtime never uses a real Wayland display basename or session.
        std::fs::create_dir(&root).unwrap();
        let state = root.join("crosspane-hypr-C2");
        let native = root.join("hypr/fake_signature");
        let process = root.join("proc").join(pid.to_string());
        for directory in [&state, &native, &process] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(state.join("env"), "export HYPRLAND_INSTANCE_SIGNATURE=fake_signature\nexport WAYLAND_DISPLAY=owned-test-socket\nexport CROSSPANE_NESTED_HYPR=1\n").unwrap();
        std::fs::write(state.join("pid"), format!("{pid}\n")).unwrap();
        std::fs::write(state.join("start"), "123\n").unwrap();
        let mut fields = vec!["S".to_owned()];
        fields.extend(std::iter::repeat_n("0".to_owned(), 18));
        fields.push("123".to_owned());
        std::fs::write(
            process.join("stat"),
            format!("{pid} (Hyprland) {}\n", fields.join(" ")),
        )
        .unwrap();
        std::fs::write(process.join("comm"), "Hyprland\n").unwrap();
        std::fs::write(
            native.join("hyprland.lock"),
            format!("{pid}\nowned-test-socket\n"),
        )
        .unwrap();
        let display_listener =
            std::os::unix::net::UnixListener::bind(root.join("owned-test-socket")).unwrap();
        display_listener.set_nonblocking(true).unwrap();
        let ipc_listener =
            std::os::unix::net::UnixListener::bind(native.join(".socket.sock")).unwrap();
        let guard = NestedConnectionGuard::new(
            root.clone(),
            root.join("proc"),
            "owned-test-socket".into(),
            "fake_signature".into(),
        );
        Self {
            guard,
            display_listener,
            _ipc_listener: ipc_listener,
        }
    }

    fn assert_not_connected(&self) {
        assert!(
            matches!(self.display_listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }
}

impl Drop for FakeNest {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.guard.runtime).unwrap();
    }
}

#[test]
fn dead_nest_socket_reuse_is_refused_before_connection() {
    let fake = FakeNest::new(std::process::id());
    assert!(fake.guard.verify().is_some());
    let _registration = fake.guard.install();
    // Leave the saved env, start and lock behind while a replacement owns the socket path.
    std::fs::remove_file(
        fake.guard
            .proc_root
            .join(fake.guard.pid.to_string())
            .join("stat"),
    )
    .unwrap();
    assert!(matches!(
        connection_guard::connect(fake.guard.runtime.join(&fake.guard.display)),
        Err(PlatformError::Backend(_))
    ));
    fake.assert_not_connected();
    assert_eq!(fake.guard.before.load(Ordering::SeqCst), 1);
    assert_eq!(fake.guard.after.load(Ordering::SeqCst), 0);
}

#[test]
fn nest_start_time_and_current_signature_lock_are_rechecked() {
    let fake = FakeNest::new(std::process::id());
    let _registration = fake.guard.install();
    let state = fake.guard.runtime.join("crosspane-hypr-C2");
    let lock = fake.guard.runtime.join("hypr/fake_signature/hyprland.lock");
    let stat = fake
        .guard
        .proc_root
        .join(fake.guard.pid.to_string())
        .join("stat");
    let recorded_stat = std::fs::read_to_string(&stat).unwrap();
    for (path, replacement) in [
        (state.join("start"), "124\n".to_owned()),
        (stat.clone(), recorded_stat.replace("123\n", "124\n")),
        (stat, recorded_stat.replace(") S ", ") Z ")),
        (
            lock.clone(),
            format!("{}\nother-test-socket\n", fake.guard.pid),
        ),
        (lock, "999999\nowned-test-socket\n".to_owned()),
    ] {
        let original = std::fs::read(&path).unwrap();
        std::fs::write(&path, replacement).unwrap();
        assert!(matches!(
            connection_guard::connect(fake.guard.runtime.join(&fake.guard.display)),
            Err(PlatformError::Backend(_))
        ));
        fake.assert_not_connected();
        std::fs::write(path, original).unwrap();
    }
    assert!(fake.guard.verify().is_some());
}

#[test]
fn replacement_peer_is_rejected_before_wayland_traffic() {
    use std::io::Read;
    let fake = FakeNest::new(std::process::id() + 1);
    assert!(fake.guard.verify().is_some());
    let _registration = fake.guard.install();
    assert!(matches!(
        connection_guard::connect(fake.guard.runtime.join(&fake.guard.display)),
        Err(PlatformError::Backend(_))
    ));
    let (mut peer, _) = fake.display_listener.accept().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    assert_eq!(
        peer.read(&mut [0; 1]).unwrap(),
        0,
        "rejected peer received protocol traffic"
    );
    fake.guard.assert_connections(1);
}

#[test]
fn named_c2_nested_watch_read_marker_empty_text_gate_timeout_rebuild_and_drop() {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: requires the owned C2 nested Hyprland");
        return;
    }
    assert!(std::env::var_os("WAYLAND_SOCKET").is_none());
    // Prove this is the specifically named nest, not merely a session with an opt-in variable.
    let runtime = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let display = std::env::var("WAYLAND_DISPLAY").unwrap();
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap();
    let guard = NestedConnectionGuard::new(runtime, "/proc".into(), display, signature.clone());
    let display_socket = guard
        .verify()
        .expect("owned C2 process, start time and signature lock");
    eprintln!(
        "C2 verified connections: backend and fixture socket={}; instance={signature}",
        display_socket.display()
    );
    if std::env::var("CROSSPANE_TEST_FD_LIMIT_CHILD").as_deref() != Ok("1") {
        // The limit applies only to this owned test child. It keeps exhaustion bounded and
        // leaves nextest, the compositor, and all owner processes unchanged.
        let name = std::thread::current().name().unwrap().to_owned();
        let status = std::process::Command::new("bash")
            .args(["-c", "ulimit -n 128; exec \"$@\"", "c2-fd-limit"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env("CROSSPANE_TEST_FD_LIMIT_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success(), "named-C2 descriptor-limit child failed");
        return;
    }

    let _registration = guard.install();
    let gate = open_gate();
    let mut backend = HyprlandClipboard::new(gate.clone(), &display_socket).unwrap();
    guard.assert_connections(1);
    let fixture = Fixture::new(display_socket);
    guard.assert_connections(2);
    eprintln!(
        "C2 initial backend and fixture peer verified: pid={}; start={}",
        guard.pid, guard.start
    );
    let (send, events) = mpsc::channel();
    backend
        .subscribe(Arc::new(move |event| {
            let _ = send.send(event);
        }))
        .unwrap();
    while events.try_recv().is_ok() {}
    let text = ClipKinds {
        text: true,
        image: false,
    };

    fixture.publish(
        types(&["text/plain"]),
        Some(b"owned fixture".to_vec()),
        false,
    );
    changed(&events, text);
    assert_eq!(backend.kinds().unwrap(), text);
    assert_eq!(
        fixture.sends(),
        0,
        "watching and kinds must not receive content"
    );
    assert!(backend.read(ClipKind::Text, 30).unwrap() == b"owned fixture");
    assert_eq!(fixture.sends(), 1);
    let mut descriptors = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => {
                descriptors.push(file);
                assert!(
                    descriptors.len() < 128,
                    "child descriptor limit was not applied"
                );
            }
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(24));
                break;
            }
        }
    }
    // Leave room for read's two pipe ends, but not Wayland's duplicated writer.
    drop(descriptors.pop().unwrap());
    drop(descriptors.pop().unwrap());
    let exhausted = backend.read(ClipKind::Text, 30);
    drop(descriptors);
    assert!(matches!(exhausted, Err(PlatformError::Backend(_))));
    assert_eq!(
        fixture.sends(),
        1,
        "failed submission must not reach the source"
    );
    // Recover through the same public handle and original subscriber, with no injected Source.
    assert_eq!(backend.kinds().unwrap(), text);
    guard.assert_connections(3);
    eprintln!(
        "C2 lazy rebuild peer verified: pid={}; start={}; pre/post checks=3/3",
        guard.pid, guard.start
    );
    changed(&events, text);
    assert!(backend.read(ClipKind::Text, 30).unwrap() == b"owned fixture");
    assert_eq!(fixture.sends(), 2);
    assert!(matches!(
        backend.read(ClipKind::Image, 30),
        Err(PlatformError::NotFound)
    ));

    fixture.publish(
        types(&["text/plain;charset=utf-8", "text/plain"]),
        Some(b"limit+1".to_vec()),
        false,
    );
    changed(&events, text);
    assert!(matches!(
        backend.read(ClipKind::Text, 6),
        Err(PlatformError::TooLarge)
    ));
    fixture.publish(types(&["STRING"]), Some(vec![0xff]), false);
    changed(&events, text);
    assert!(matches!(
        backend.read(ClipKind::Text, 6),
        Err(PlatformError::NotFound)
    ));
    // A genuine empty-text source is also unavailable under the lead's EOF mapping ruling.
    fixture.publish(types(&["text/plain"]), Some(Vec::new()), false);
    changed(&events, text);
    assert_eq!(backend.kinds().unwrap(), text);
    assert!(matches!(
        backend.read(ClipKind::Text, 30),
        Err(PlatformError::NotFound)
    ));

    while events.try_recv().is_ok() {}
    fixture.publish(
        types(&["text/plain", &backend.marker]),
        Some(b"own marker".to_vec()),
        false,
    );
    assert_eq!(backend.kinds().unwrap(), text);
    assert!(
        events.recv_timeout(Duration::from_millis(100)).is_err(),
        "own marker must suppress Changed"
    );
    assert!(matches!(
        backend.read(ClipKind::Text, 30),
        Err(PlatformError::NotFound)
    ));
    assert!(!format!("{backend:?}").contains(&backend.marker));

    fixture.publish(types(&["text/plain"]), None, false);
    changed(&events, text);
    gate.set_engine_permits(false);
    assert_eq!(backend.kinds().unwrap(), text);
    assert!(matches!(
        backend.read(ClipKind::Text, 30),
        Err(PlatformError::Locked)
    ));
    assert!(matches!(
        backend.promise(1, text),
        Err(PlatformError::Locked)
    ));
    gate.set_engine_permits(true);
    let start = Instant::now();
    assert!(matches!(
        backend.read(ClipKind::Text, 30),
        Err(PlatformError::Timeout)
    ));
    assert!(
        start.elapsed() >= Duration::from_millis(1400)
            && start.elapsed() < Duration::from_millis(1800)
    );

    fixture.publish(
        types(&["text/plain"]),
        Some(b"clear fixture".to_vec()),
        false,
    );
    changed(&events, text);
    assert!(backend.read(ClipKind::Text, 30).unwrap() == b"clear fixture");
    fixture.publish(Vec::new(), None, false);
    // [E] Hyprland clears the source without an ext-data-control NULL selection event.
    // Its stale offer must not supply the previous clipboard content.
    let cleared = backend.read(ClipKind::Text, 30);
    match &cleared {
        Ok(bytes) => eprintln!("cleared-source read: success, size={}", bytes.len()),
        Err(error) => eprintln!(
            "cleared-source read: {}",
            match error {
                PlatformError::NotFound => "NotFound",
                PlatformError::Timeout => "Timeout",
                PlatformError::Locked => "Locked",
                PlatformError::TooLarge => "TooLarge",
                PlatformError::Backend(_) => "Backend",
                _ => "other error",
            }
        ),
    }
    assert!(matches!(cleared, Err(PlatformError::NotFound)));
    fixture.publish(types(&["unrecognized/type"]), Some(Vec::new()), false);
    changed(&events, ClipKinds::default());
    assert!(matches!(
        backend.read(ClipKind::Text, 30),
        Err(PlatformError::NotFound)
    ));
    fixture.publish(
        types(&["text/plain"]),
        Some(b"primary fixture".to_vec()),
        true,
    );
    assert_eq!(backend.kinds().unwrap(), ClipKinds::default());
    assert!(
        events.recv_timeout(Duration::from_millis(100)).is_err(),
        "primary selection must be ignored"
    );
    let start = Instant::now();
    drop(backend);
    assert!(start.elapsed() < Duration::from_millis(200));
    assert!(events.try_recv().is_err());
    guard.assert_connections(3);
    eprintln!(
        "C2a: metadata-only watch, read, marker, limit, UTF-8, gate, 1.5s timeout, connection rebuild, subscriber preservation, primary isolation and drop passed"
    );
}
