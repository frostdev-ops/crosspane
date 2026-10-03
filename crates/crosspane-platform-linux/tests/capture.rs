//! Self-driven capture conformance in a nested Hyprland only. Never use the owner's pointer.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]
use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, Edge, EndReason, InputCapture, IoGate, MotionKind,
    PlatformError, PortalId,
};
use crosspane_platform_linux::hyprland::{
    capture::{
        HyprlandCapture, NestEndpoints, NestProof, PortalsTestHooks, SetPortalsFailure,
        set_portals_failure, verify_nest,
    },
    cursor_position,
    ipc::HyprIpc,
};
use crosspane_types::{
    geom::PointDevice,
    hid::{HidUsage, MouseButton},
    id::DisplayId,
    input::ScrollPhase,
};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::fd::AsFd,
    path::PathBuf,
    process::Command,
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry,
        wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::{
    layer_shell::v1::client::{
        zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
        zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
    },
    virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    },
};
use xkbcommon::xkb;
fn lock_file(name: &str, operation: rustix::fs::FlockOperation) -> File {
    let path = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join(name);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    rustix::fs::flock(&lock, operation).unwrap();
    lock
}
/// Held while a test uses the shared nest. Nextest launches separate processes; an in-process
/// mutex cannot protect the shared nest.
///
/// A test that starts another nest (or creates an output) re-tiles every nested window in the
/// parent compositor and resizes this nest's output under whatever is running in it. The tests
/// that need their own nest (`dedicated`) take the topology lock exclusively; every test that uses
/// the shared nest holds it shared (and this nest's lock exclusively, so only one of them runs at
/// a time, `compositor_going_away` with its nest included). The two kinds never overlap. A test
/// running inside its own nest holds none of this: its parent process holds the exclusive lock.
fn serialize() -> (File, Option<File>) {
    let shared = std::env::var_os("CROSSPANE_CAPTURE_CHILD")
        .is_none()
        .then(|| {
            lock_file(
                "crosspane-capture-topology.lock",
                rustix::fs::FlockOperation::LockShared,
            )
        });
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap();
    (
        lock_file(
            &format!("crosspane-capture-{signature}.lock"),
            rustix::fs::FlockOperation::LockExclusive,
        ),
        shared,
    )
}
/// Whether the compositor tests run. Without `CROSSPANE_NESTED_HYPR=1` they skip with a message.
/// With it, every endpoint the process would reach is verified to belong to a nest started by
/// `scripts/hypr-nested.sh` ([`verify_nest`]) and the test **panics** if not: these tests move
/// pointers, press keys and click, so a wrong endpoint (the owner's live session) must never be
/// talked to, not merely skipped. Every test calls this before it connects to anything.
fn nested() -> bool {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: capture needs CROSSPANE_NESTED_HYPR=1 from scripts/hypr-nested.sh env");
        return false;
    }
    require_nest();
    true
}

/// The proof that this process's environment addresses a nest that `scripts/hypr-nested.sh`
/// started (`verify_nest`, in the library: it reads files and connects to nothing). Panics if it
/// does not. A proof is the only way to build a backend whose test hooks work
/// (`HyprlandCapture::new_for_nest_test`), and only `verify_nest` makes one.
fn require_nest() -> NestProof {
    match verify_nest(&NestEndpoints::from_process()) {
        Ok(proof) => proof,
        Err(why) => panic!(
            "refusing to run against a compositor that is not a nest from scripts/hypr-nested.sh: {why}"
        ),
    }
}
#[derive(Default)]
struct DriverState {
    width: u32,
    height: u32,
    size: Option<(u32, u32)>,
    sync: u64,
    keyboard: bool,
    pointer_pos: Option<(f64, f64)>,
}
struct Driver {
    conn: Connection,
    queue: EventQueue<DriverState>,
    state: DriverState,
    qh: QueueHandle<DriverState>,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    token: u64,
    /// The observer layer surface (a full-output overlay that gives this client keyboard focus
    /// and sees the pointer); absent for a [`Driver::bare`] one.
    _observer: Option<(
        wl_surface::WlSurface,
        ZwlrLayerSurfaceV1,
        wl_buffer::WlBuffer,
    )>,
}
impl Driver {
    fn new() -> Self {
        Self::build(true)
    }
    /// Virtual pointer and keyboard only: no observer surface covers the output, so another
    /// client's window can receive (or not receive) the injected input.
    fn bare() -> Self {
        Self::build(false)
    }
    fn build(observer: bool) -> Self {
        assert!(nested());
        let conn = Connection::connect_to_env().unwrap();
        let (globals, queue) = registry_queue_init::<DriverState>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 7..=9, ()).unwrap();
        let output: wl_output::WlOutput = globals.bind(&qh, 4..=4, ()).unwrap();
        let shell: ZwlrLayerShellV1 = globals.bind(&qh, 3..=5, ()).unwrap();
        let _local_keyboard = seat.get_keyboard(&qh, ());
        let _local_pointer = seat.get_pointer(&qh, ());
        let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 2..=2, ()).unwrap();
        let pointer =
            manager.create_virtual_pointer_with_output(Some(&seat), Some(&output), &qh, ());
        let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
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
            rustix::fs::memfd_create("capture-test-keymap", rustix::fs::MemfdFlags::CLOEXEC)
                .unwrap(),
        );
        file.write_all(&text).unwrap();
        keyboard.keymap(1, file.as_fd(), text.len() as u32);
        let mut state = DriverState::default();
        let mut queue = queue;
        let deadline = Instant::now() + Duration::from_secs(2);
        let observer = if observer {
            let surface = compositor.create_surface(&qh, ());
            let layer = shell.get_layer_surface(
                &surface,
                Some(&output),
                Layer::Overlay,
                "crosspane-capture-test-observer".into(),
                &qh,
                (),
            );
            layer.set_size(0, 0);
            layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
            surface.commit();
            while state.size.is_none() || state.width == 0 {
                driver_pump(&conn, &mut queue, &mut state);
                assert!(Instant::now() < deadline, "observer configure timed out");
            }
            let (width, height) = state.size.unwrap();
            let file = File::from(
                rustix::fs::memfd_create("capture-test-observer", rustix::fs::MemfdFlags::CLOEXEC)
                    .unwrap(),
            );
            file.set_len(u64::from(width) * u64::from(height) * 4)
                .unwrap();
            let pool = shm.create_pool(file.as_fd(), (width * height * 4) as i32, &qh, ());
            let buffer = pool.create_buffer(
                0,
                width as i32,
                height as i32,
                (width * 4) as i32,
                wl_shm::Format::Argb8888,
                &qh,
                (),
            );
            surface.attach(Some(&buffer), 0, 0);
            surface.commit();
            pool.destroy();
            Some((surface, layer, buffer))
        } else {
            while state.width == 0 {
                driver_pump(&conn, &mut queue, &mut state);
                assert!(Instant::now() < deadline, "output mode timed out");
            }
            None
        };
        let has_observer = observer.is_some();
        let mut d = Self {
            conn,
            queue,
            state,
            qh,
            pointer,
            keyboard,
            token: 0,
            _observer: observer,
        };
        d.sync();
        if has_observer {
            d.focus();
        }
        d
    }
    fn time(&self) -> u32 {
        {
            let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
            (time.tv_sec as u64 * 1000 + time.tv_nsec as u64 / 1_000_000) as u32
        }
    }
    fn sync(&mut self) {
        self.token += 1;
        self.conn.display().sync(&self.qh, self.token);
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.state.sync < self.token {
            driver_pump(&self.conn, &mut self.queue, &mut self.state);
            assert!(Instant::now() < deadline, "driver sync timed out");
        }
    }
    fn absolute(&mut self, edge: Edge, on: bool) {
        self.sync();
        let (w, h) = (self.state.width, self.state.height);
        let (x, y) = if on {
            match edge {
                Edge::Left => (0, h / 2),
                Edge::Right => (w - 1, h / 2),
                Edge::Top => (w / 2, 0),
                Edge::Bottom => (w / 2, h - 1),
            }
        } else {
            (w / 2, h / 2)
        };
        self.pointer.motion_absolute(self.time(), x, y, w, h);
        self.pointer.frame();
        self.sync();
    }
    /// Warp to an absolute pixel of the driver's output.
    fn absolute_at(&mut self, x: u32, y: u32) {
        self.sync();
        let (w, h) = (self.state.width, self.state.height);
        self.pointer
            .motion_absolute(self.time(), x.min(w - 1), y.min(h - 1), w, h);
        self.pointer.frame();
        self.sync();
    }
    fn motion(&mut self, dx: f64, dy: f64) {
        self.pointer.motion(self.time(), dx, dy);
        self.pointer.frame();
        self.sync();
    }
    fn button(&mut self, down: bool) {
        self.pointer.button(
            self.time(),
            0x110,
            if down {
                wl_pointer::ButtonState::Pressed
            } else {
                wl_pointer::ButtonState::Released
            },
        );
        self.pointer.frame();
        self.sync();
    }
    fn focus(&mut self) {
        self.absolute(Edge::Right, false);
        self.button(true);
        self.button(false);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.state.keyboard {
            driver_pump(&self.conn, &mut self.queue, &mut self.state);
            assert!(Instant::now() < deadline, "observer focus timed out");
        }
    }
    fn shift(&mut self, down: bool) {
        self.keyboard.key(self.time(), 42, u32::from(down));
        self.keyboard.modifiers(u32::from(down), 0, 0, 0);
        self.sync();
    }
}
fn driver_pump<S>(conn: &Connection, queue: &mut EventQueue<S>, state: &mut S) {
    queue.dispatch_pending(state).unwrap();
    conn.flush().unwrap();
    if let Some(guard) = conn.prepare_read() {
        let mut fds = [PollFd::new(conn, PollFlags::IN)];
        let timeout = Timespec::try_from(Duration::from_millis(1)).unwrap();
        if poll(&mut fds, Some(&timeout)).unwrap() > 0 {
            guard.read().unwrap();
        }
    }
    queue.dispatch_pending(state).unwrap();
}
struct Fixture {
    driver: Driver,
    capture: HyprlandCapture,
    gate: Arc<IoGate>,
    events: mpsc::Receiver<CaptureEvent>,
    portal: CapturePortal,
    /// Where along a right-edge portal `enter` presses it; the middle of the output if `None`.
    /// Set it when the portal is a short stretch that doesn't reach the middle.
    entry_y: Option<u32>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_sink(Arc::new(|_| {}))
    }
    fn with_sink(tap: Arc<dyn Fn(&CaptureEvent) + Send + Sync>) -> Self {
        Self::with_driver(Driver::new(), tap)
    }
    fn with_driver(mut driver: Driver, tap: Arc<dyn Fn(&CaptureEvent) + Send + Sync>) -> Self {
        driver.sync();
        let id = crosspane_types::id::DisplayId(
            HyprIpc::from_env().unwrap().monitor_ids().unwrap()[0].1,
        );
        let portal = CapturePortal {
            id: PortalId(1),
            display: id,
            edge: Edge::Right,
            from: f64::from(driver.state.height) / 4.0,
            to: f64::from(driver.state.height) * 3.0 / 4.0,
        };
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        // Built from a proof, verified before it connects: the only way to get working hooks.
        let mut capture =
            HyprlandCapture::new_for_nest_test(gate.clone(), &require_nest()).unwrap();
        let (tx, events) = mpsc::channel();
        capture
            .subscribe(Arc::new(move |e| {
                tap(&e);
                let _ = tx.send(e);
            }))
            .unwrap();
        let initial = events.recv_timeout(Duration::from_secs(1)).unwrap();
        if let CaptureEvent::LockKeys(locks) = initial {
            assert!(
                locks.caps_lock.is_some()
                    || locks.num_lock.is_some()
                    || locks.scroll_lock.is_some()
            );
            assert_eq!(
                events.recv_timeout(Duration::from_secs(1)).unwrap(),
                CaptureEvent::KeyboardBlinded(false)
            );
        } else {
            assert_eq!(initial, CaptureEvent::KeyboardBlinded(false));
        }
        capture.set_portals(&[portal]).unwrap();
        Self {
            driver,
            capture,
            gate,
            events,
            portal,
            entry_y: None,
        }
    }
    fn press(&mut self) {
        match self.entry_y {
            Some(y) => self.driver.absolute_at(self.driver.state.width - 1, y),
            None => self.driver.absolute(self.portal.edge, true),
        }
    }
    fn enter(&mut self) {
        self.driver.absolute(self.portal.edge, false);
        // Drain old idle events before this crossing, never while a capture is active.
        while self.events.try_recv().is_ok() {}
        self.press();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.events.recv_timeout(Duration::from_millis(20)) {
                Ok(CaptureEvent::EdgePressed {
                    portal, position, ..
                }) if portal == self.portal.id => {
                    assert!((0.0..=1.0).contains(&position));
                    return;
                }
                Ok(_) => (),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "edge did not recover");
                    // Hyprland does not refocus a newly mapped strip under a stationary pointer.
                    // Keep crossing while automatic reconnection is backing off; no API command.
                    self.driver.absolute(self.portal.edge, false);
                    self.press();
                }
                Err(e) => panic!("edge event channel failed: {e}"),
            }
        }
    }

    fn wait(&self, predicate: impl Fn(&CaptureEvent) -> bool) -> CaptureEvent {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let e = self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if predicate(&e) {
                return e;
            }
        }
    }
    fn start(&mut self, id: CaptureId) {
        let before = Instant::now();
        self.capture.begin(id, self.portal.id).unwrap();
        assert!(before.elapsed() < Duration::from_millis(50));
        assert_eq!(
            self.wait(|e| matches!(e, CaptureEvent::Started { .. })),
            CaptureEvent::Started { id }
        );
    }
    fn ended(&self, id: CaptureId, reason: EndReason) {
        assert_eq!(
            self.wait(|e| matches!(e, CaptureEvent::Ended { .. })),
            CaptureEvent::Ended { id, reason }
        );
    }
}

#[test]
fn hundred_crossings() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    let mut activation_max = Duration::ZERO;
    for n in 0..100 {
        if n % 25 == 0 && n != 0 {
            f.portal.edge = match n {
                25 => Edge::Left,
                50 => Edge::Top,
                _ => Edge::Bottom,
            };
            let length = if matches!(f.portal.edge, Edge::Left | Edge::Right) {
                f.driver.state.height
            } else {
                f.driver.state.width
            };
            f.portal.from = f64::from(length) / 4.0;
            f.portal.to = f64::from(length) * 3.0 / 4.0;
            f.capture.set_portals(&[f.portal]).unwrap();
        }
        f.enter();
        let id = CaptureId(n + 1);
        let before = Instant::now();
        f.start(id);
        activation_max = activation_max.max(before.elapsed());
        for sample in 0..20 {
            let (dx, dy) = (1.0 + f64::from(sample % 3), -1.0);
            let time = match sample {
                0 => 0,
                1 => f.driver.time().wrapping_sub(5000),
                _ => f.driver.time(),
            };
            f.driver.pointer.motion(time, dx, dy);
            f.driver.pointer.frame();
            f.driver.sync();
            let e = loop {
                let e = f.events.recv_timeout(Duration::from_secs(1)).unwrap();
                if !matches!(e, CaptureEvent::LockKeys(_)) {
                    break e;
                }
            };
            match e {
                CaptureEvent::Motion {
                    dx: x,
                    dy: y,
                    kind,
                    at,
                } => {
                    assert_eq!((x, y), (dx, dy));
                    assert_eq!(kind, MotionKind::Unaccelerated);
                    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
                    let clock = time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64;
                    assert!(
                        clock.abs_diff(at.as_nanos()) < 250_000_000,
                        "motion timestamp is not on CLOCK_MONOTONIC"
                    );
                }
                other => panic!("unexpected event between capture fences: {other:?}"),
            }
        }
        let center = PointDevice::new(
            f64::from(f.driver.state.width) / 2.0,
            f64::from(f.driver.state.height) / 2.0,
        );
        f.capture.end(Some((f.portal.display, center))).unwrap();
        f.ended(id, EndReason::Requested);
        f.driver.sync();
        let (x, y) = f.driver.state.pointer_pos.unwrap();
        assert!(
            (x - center.x).abs() <= 1.0 && (y - center.y).abs() <= 1.0,
            "end did not warp to center"
        );
        for e in f.events.try_iter() {
            assert!(!matches!(
                e,
                CaptureEvent::Motion { .. }
                    | CaptureEvent::Key { .. }
                    | CaptureEvent::Button { .. }
                    | CaptureEvent::Scroll { .. }
            ));
        }
    }
    eprintln!(
        "100 crossings across four edges, 2000 exact motion samples; max activation {:?}",
        activation_max
    );
}

#[test]
fn closed_gate_and_button_guard() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    f.enter();
    f.gate.set_engine_permits(false);
    assert!(matches!(
        f.capture.begin(CaptureId(1), f.portal.id),
        Err(PlatformError::Locked)
    ));
    f.gate.set_engine_permits(true);
    f.driver.button(true);
    assert!(matches!(
        f.capture.begin(CaptureId(2), f.portal.id),
        Err(PlatformError::PointerButtonHeld)
    ));
    f.driver.button(false);
    f.start(CaptureId(3));
    f.driver.shift(true);
    assert!(
        matches!(f.wait(|e|matches!(e,CaptureEvent::Key {..})),CaptureEvent::Key {usage,down:true,..} if usage == HidUsage::keyboard(0xE1))
    );
    f.driver.shift(false);
    assert!(matches!(
        f.wait(|e| matches!(e, CaptureEvent::Key { .. })),
        CaptureEvent::Key { down: false, .. }
    ));
    // The test US keymap's Lock modifier is bit 1. Assert the lock snapshot follows the key
    // that caused it, rather than overtaking it in the subscription's delivery thread.
    for locked in [true, false] {
        f.driver.keyboard.key(f.driver.time(), 58, 1);
        f.driver
            .keyboard
            .modifiers(0, 0, if locked { 2 } else { 0 }, 0);
        f.driver.keyboard.key(f.driver.time(), 58, 0);
        f.driver.sync();
        assert!(
            matches!(f.wait(|e|matches!(e,CaptureEvent::Key {..})),CaptureEvent::Key {usage,down:true,..} if usage == HidUsage::keyboard(0x39))
        );
        assert!(
            matches!(f.events.recv_timeout(Duration::from_secs(1)).unwrap(),CaptureEvent::LockKeys(locks) if locks.caps_lock == Some(locked) && locks.scroll_lock.is_none())
        );
        assert!(
            matches!(f.events.recv_timeout(Duration::from_secs(1)).unwrap(),CaptureEvent::Key {usage,down:false,..} if usage == HidUsage::keyboard(0x39))
        );
    }
    f.driver.button(true);
    assert!(matches!(
        f.wait(|e| matches!(e, CaptureEvent::Button { .. })),
        CaptureEvent::Button { down: true, .. }
    ));
    f.driver.button(false);
    assert!(matches!(
        f.wait(|e| matches!(e, CaptureEvent::Button { .. })),
        CaptureEvent::Button { down: false, .. }
    ));
    for (code, expected) in [
        (0x113, crosspane_types::hid::MouseButton::BACK),
        (0x114, crosspane_types::hid::MouseButton::FORWARD),
    ] {
        for down in [true, false] {
            f.driver.pointer.button(
                0,
                code,
                if down {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                },
            );
            f.driver.pointer.frame();
            f.driver.sync();
            assert!(
                matches!(f.wait(|e| matches!(e, CaptureEvent::Button { .. })), CaptureEvent::Button { button, down: state, at } if button == expected && state == down && at.as_nanos() > 0)
            );
        }
    }
    for code in [0x115, 0x116] {
        f.driver
            .pointer
            .button(f.driver.time(), code, wl_pointer::ButtonState::Pressed);
        f.driver
            .pointer
            .button(f.driver.time(), code, wl_pointer::ButtonState::Released);
        f.driver.pointer.frame();
    }
    f.driver.sync();
    f.driver.pointer.axis_source(wl_pointer::AxisSource::Wheel);
    f.driver
        .pointer
        .axis_discrete(f.driver.time(), wl_pointer::Axis::VerticalScroll, 10.0, 1);
    f.driver.pointer.frame();
    f.driver.sync();
    assert!(
        matches!(f.events.recv_timeout(Duration::from_secs(1)).unwrap(),CaptureEvent::Scroll {delta,..} if delta.v120_y == -120 && delta.pixels.is_none() && delta.phase == ScrollPhase::Discrete)
    );
    f.driver
        .pointer
        .axis(f.driver.time(), wl_pointer::Axis::VerticalScroll, 1.5);
    // The virtual-pointer axis request resets source; set it after axis/stop for each frame.
    f.driver.pointer.axis_source(wl_pointer::AxisSource::Finger);
    f.driver.pointer.frame();
    f.driver.sync();
    let injected = f.wait(|e| matches!(e, CaptureEvent::Scroll { .. }));
    assert!(
        matches!(injected,CaptureEvent::Scroll {delta,..} if delta.pixels.is_some_and(|v|v.y == -1.5) && delta.phase == ScrollPhase::Began)
    );
    f.driver
        .pointer
        .axis(f.driver.time(), wl_pointer::Axis::HorizontalScroll, 3.0);
    // The virtual-pointer axis request resets source; set it after axis/stop for each frame.
    f.driver.pointer.axis_source(wl_pointer::AxisSource::Finger);
    f.driver.pointer.frame();
    f.driver.sync();
    assert!(
        matches!(f.wait(|e|matches!(e,CaptureEvent::Scroll {..})),CaptureEvent::Scroll {delta,..} if delta.pixels.unwrap().x == 3.0 && delta.phase == ScrollPhase::Changed)
    );
    f.driver
        .pointer
        .axis_stop(f.driver.time(), wl_pointer::Axis::VerticalScroll);
    // The virtual-pointer axis request resets source; set it after axis/stop for each frame.
    f.driver.pointer.axis_source(wl_pointer::AxisSource::Finger);
    f.driver.pointer.frame();
    f.driver.sync();
    assert!(
        matches!(f.wait(|e|matches!(e,CaptureEvent::Scroll {..})),CaptureEvent::Scroll {delta,..} if delta.stop_y && delta.phase == ScrollPhase::Changed)
    );
    f.driver
        .pointer
        .axis_stop(f.driver.time(), wl_pointer::Axis::HorizontalScroll);
    // The virtual-pointer axis request resets source; set it after axis/stop for each frame.
    f.driver.pointer.axis_source(wl_pointer::AxisSource::Finger);
    f.driver.pointer.frame();
    f.driver.sync();
    assert!(
        matches!(f.wait(|e|matches!(e,CaptureEvent::Scroll {..})),CaptureEvent::Scroll {delta,..} if delta.stop_x && delta.phase == ScrollPhase::Ended)
    );
    f.gate.set_engine_permits(false);
    f.ended(CaptureId(3), EndReason::Lost);
    // A closed gate skips even an invalid warp target, while unlock still succeeds.
    f.capture
        .end(Some((
            f.portal.display,
            PointDevice::new(f64::NAN, f64::NAN),
        )))
        .unwrap();
    f.driver.absolute(f.portal.edge, false);
}

fn assert_pointer_free(f: &mut Fixture, before: Instant, label: &str) {
    while before.elapsed() < Duration::from_millis(45) {
        f.driver.absolute(f.portal.edge, false);
        if f.driver.state.pointer_pos.is_some() {
            assert!(before.elapsed() < Duration::from_millis(50));
            eprintln!(
                "{label}: observer pointer focus restored in {:?}",
                before.elapsed()
            );
            return;
        }
    }
    panic!("{label}: pointer stayed locked for 50 ms");
}

#[test]
fn worker_error_releases_healthy_socket() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    f.enter();
    f.start(CaptureId(1));
    assert!(f.driver.state.pointer_pos.is_none());
    let before = Instant::now();
    // This makes pump fail while its Wayland socket is still healthy (like a keymap error).
    f.capture.inject_worker_error_for_test().unwrap();
    f.ended(CaptureId(1), EndReason::Lost);
    assert_pointer_free(&mut f, before, "worker error");
    // Failure recovery, like abort recovery, must restore strips without an API command.
    f.enter();
    f.start(CaptureId(2));
    f.capture.end(None).unwrap();
    f.ended(CaptureId(2), EndReason::Requested);
}

#[test]
fn sink_panic_releases_capture() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::with_sink(Arc::new(|e| {
        if matches!(e, CaptureEvent::Motion { .. }) {
            panic!("test sink panic");
        }
    }));
    f.enter();
    f.start(CaptureId(1));
    let before = Instant::now();
    f.driver.motion(1.0, 0.0);
    assert_pointer_free(&mut f, before, "sink panic");
}

#[test]
fn stalled_sink_releases_capture() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    struct Resume(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Resume {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let pause = Arc::new((Mutex::new(false), Condvar::new()));
    let resume = Resume(pause.clone());
    let (blocked, ready) = mpsc::channel();
    let mut f = Fixture::with_sink(Arc::new(move |e| {
        if matches!(e, CaptureEvent::Motion { .. }) {
            let mut released = pause.0.lock().unwrap();
            if !*released {
                let _ = blocked.send(());
                while !*released {
                    released = pause.1.wait(released).unwrap();
                }
            }
        }
    }));
    f.enter();
    f.start(CaptureId(1));
    f.driver.motion(1.0, 0.0);
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    let before = Instant::now();
    // Fill the bounded event socket while the consumer is stuck inside send().
    for _ in 0..2048 {
        f.driver.pointer.motion(f.driver.time(), 1.0, 0.0);
        f.driver.pointer.frame();
    }
    f.driver.sync();
    assert_pointer_free(&mut f, before, "stalled sink");
    drop(resume);
    f.ended(CaptureId(1), EndReason::Lost);
}
#[test]
fn abort_from_another_thread() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    f.enter();
    f.start(CaptureId(1));
    let handle = f.capture.abort_handle();
    let before = Instant::now();
    std::thread::spawn(move || {
        handle.abort();
        handle.abort();
    })
    .join()
    .unwrap();
    f.ended(CaptureId(1), EndReason::Aborted);
    f.driver.absolute(f.portal.edge, false);
    while f.driver.state.pointer_pos.is_none() && before.elapsed() < Duration::from_millis(45) {
        driver_pump(&f.driver.conn, &mut f.driver.queue, &mut f.driver.state);
    }
    assert!(
        f.driver.state.pointer_pos.is_some(),
        "pointer remained locked after abort"
    );
    assert!(
        before.elapsed() < Duration::from_millis(50),
        "abort did not free pointer within 50 ms"
    );
    eprintln!(
        "abort: Ended Aborted and observer pointer focus restored in {:?}",
        before.elapsed()
    );
    // No command triggers reconnection: the next edge press must arrive autonomously.
    f.enter();
    f.start(CaptureId(2));
    f.capture.end(None).unwrap();
    f.ended(CaptureId(2), EndReason::Requested);
}
#[test]
fn portal_replacement_is_atomic() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    f.enter();
    let bad = CapturePortal {
        id: PortalId(2),
        to: f64::NAN,
        ..f.portal
    };
    assert!(f.capture.set_portals(&[f.portal, bad]).is_err());
    f.start(CaptureId(1));
    f.capture.end(None).unwrap();
    f.ended(CaptureId(1), EndReason::Requested);
    // Removing a portal cancels a push-to-cross; only the replacement receives the next crossing.
    f.portal = CapturePortal {
        id: PortalId(3),
        edge: Edge::Left,
        ..f.portal
    };
    f.capture.set_portals(&[f.portal]).unwrap();
    f.wait(|e| {
        matches!(
            e,
            CaptureEvent::EdgeReleased {
                portal: PortalId(1),
                ..
            }
        )
    });
    f.enter();
    assert!(matches!(
        f.capture.begin(CaptureId(2), PortalId(1)),
        Err(PlatformError::NotFound)
    ));
    f.start(CaptureId(3));
    // An identical set keeps the active strip, and with it the capture (WP-2.43d); only a change
    // to the captured strip's own portal ends the capture.
    f.capture.set_portals(&[f.portal]).unwrap();
    assert!(
        f.events
            .recv_timeout(Duration::from_millis(100))
            .is_err_and(|e| e == mpsc::RecvTimeoutError::Timeout),
        "an identical portal set ended or disturbed the capture"
    );
    f.capture
        .set_portals(&[CapturePortal {
            to: f.portal.to - 8.0,
            ..f.portal
        }])
        .unwrap();
    f.ended(CaptureId(3), EndReason::Lost);
    // Fractional device endpoints must not activate from the rounded surface's padding.
    f.portal = CapturePortal {
        id: PortalId(4),
        edge: Edge::Right,
        from: 1.5,
        to: 3.5,
        ..f.portal
    };
    f.capture.set_portals(&[f.portal]).unwrap();
    f.driver.pointer.motion_absolute(
        f.driver.time(),
        f.driver.state.width - 1,
        1,
        f.driver.state.width,
        f.driver.state.height,
    );
    f.driver.pointer.frame();
    f.driver.sync();
    assert!(matches!(
        f.capture.begin(CaptureId(4), f.portal.id),
        Err(PlatformError::NotFound)
    ));
    f.driver.motion(0.0, 1.0);
    assert!(
        matches!(f.wait(|e|matches!(e,CaptureEvent::EdgePressed {portal:PortalId(4),..})),CaptureEvent::EdgePressed {position,..} if (position-0.25).abs()<0.01)
    );
    f.start(CaptureId(5));
    f.capture.end(None).unwrap();
    f.ended(CaptureId(5), EndReason::Requested);
}

#[test]
fn preexisting_shift_reports_snapshot_and_release() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    f.driver.focus();
    f.driver.shift(true);
    f.enter();
    let start = f.capture.begin(CaptureId(1), f.portal.id).unwrap();
    assert!(start.held_keys.contains(&HidUsage::keyboard(0xE1)));
    assert_eq!(
        f.wait(|e| matches!(e, CaptureEvent::Started { .. })),
        CaptureEvent::Started { id: CaptureId(1) }
    );
    assert!(
        !f.events
            .try_iter()
            .any(|e| matches!(e, CaptureEvent::Key { down: true, .. }))
    );
    f.driver.shift(false);
    assert!(
        matches!(f.wait(|e|matches!(e,CaptureEvent::Key {..})),CaptureEvent::Key {usage,down:false,..} if usage == HidUsage::keyboard(0xE1))
    );
    f.capture.end(None).unwrap();
    f.ended(CaptureId(1), EndReason::Requested);
}
#[test]
fn compositor_going_away() {
    if !nested() {
        return;
    }
    if std::env::var("CROSSPANE_CAPTURE_LOSS_CHILD").as_deref() == Ok("1") {
        let mut f = Fixture::new();
        f.enter();
        f.start(CaptureId(1));
        let _ = HyprIpc::from_env()
            .unwrap()
            .eval("hl.dispatch(hl.dsp.exit())");
        f.ended(CaptureId(1), EndReason::Lost);
        return;
    }
    let _guard = serialize();
    // Give the destructive loss test its own named nest and process-local environment. Never
    // stop the instance used by other acceptance tests, and never mutate process-wide env.
    let name = format!("wp-1-14-loss-{}", std::process::id());
    assert!(
        Command::new(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/hypr-nested.sh")
        )
        .args(["start", "--name", &name])
        .status()
        .unwrap()
        .success()
    );
    struct Stop(String);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = Command::new(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/hypr-nested.sh"),
            )
            .args(["stop", "--name", &self.0])
            .status();
        }
    }
    let _stop = Stop(name.clone());
    let status = Command::new("bash")
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .arg("-c")
        .arg("eval \"$(scripts/hypr-nested.sh env --name \"$1\")\"; shift; exec \"$@\"")
        .arg("capture-loss")
        .arg(name)
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "compositor_going_away", "--nocapture"])
        .env("CROSSPANE_CAPTURE_LOSS_CHILD", "1")
        .status()
        .unwrap();
    assert!(status.success());
}
// ------------------------------------------------------------------------------------------------
// WP-2.43d: strip preservation, topology during a capture, installation truth, warps.
// ------------------------------------------------------------------------------------------------

fn ipc() -> HyprIpc {
    HyprIpc::from_env().unwrap()
}

/// One of the backend's strips as the compositor lists it (`hyprctl layers`).
#[derive(Clone, Debug, PartialEq)]
struct StripInfo {
    monitor: String,
    address: String,
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

/// Every strip of this backend that the compositor currently lists, ordered by address.
fn strips(ipc: &HyprIpc) -> Vec<StripInfo> {
    let layers = ipc.json("layers").unwrap();
    let mut found = Vec::new();
    for (monitor, entry) in layers.as_object().unwrap() {
        for level in entry["levels"].as_object().unwrap().values() {
            for layer in level.as_array().unwrap() {
                if layer["namespace"] == "crosspane-capture" {
                    let number = |key: &str| layer[key].as_i64().unwrap();
                    found.push(StripInfo {
                        monitor: monitor.clone(),
                        address: layer["address"].as_str().unwrap().to_owned(),
                        x: number("x"),
                        y: number("y"),
                        w: number("w"),
                        h: number("h"),
                    });
                }
            }
        }
    }
    found.sort_by(|a, b| a.address.cmp(&b.address));
    found
}

/// The compositor handles a client's destroy requests on its own schedule: wait until exactly
/// `count` strips are listed.
fn wait_strips(ipc: &HyprIpc, count: usize) -> Vec<StripInfo> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let listed = strips(ipc);
        if listed.len() == count {
            return listed;
        }
        assert!(
            Instant::now() < deadline,
            "expected {count} strips, the compositor lists {listed:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The strip on `edge` of the nest's first output (which sits at the origin), if one is listed.
fn on_edge(listed: &[StripInfo], edge: Edge, width: u32, height: u32) -> Option<&StripInfo> {
    let (width, height) = (i64::from(width), i64::from(height));
    listed.iter().find(|s| match edge {
        Edge::Right => s.w == 1 && s.x == width - 1,
        Edge::Left => s.w == 1 && s.x == 0,
        Edge::Top => s.h == 1 && s.y == 0,
        Edge::Bottom => s.h == 1 && s.y == height - 1,
    })
}

/// How many descriptors the nested compositor holds (the same helper as `inject.rs`).
fn compositor_fd_count() -> usize {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap();
    let state = std::fs::read_dir(runtime)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("crosspane-hypr-")
                && std::fs::read_to_string(path.join("env"))
                    .unwrap_or_default()
                    .lines()
                    .any(|line| line == format!("export HYPRLAND_INSTANCE_SIGNATURE={signature}"))
        })
        .unwrap();
    let pid: u32 = std::fs::read_to_string(state.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .count()
}

/// The compositor's descriptor count once it holds still (a short-lived IPC connection of ours
/// may not be closed on its side yet).
fn settled_fd_count() -> usize {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut last = compositor_fd_count();
    loop {
        std::thread::sleep(Duration::from_millis(40));
        let now = compositor_fd_count();
        if now == last || Instant::now() >= deadline {
            return now;
        }
        last = now;
    }
}

#[derive(Clone, Debug)]
struct MonitorInfo {
    id: u32,
    name: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
}

fn monitors(ipc: &HyprIpc) -> Vec<MonitorInfo> {
    ipc.json("monitors")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|m| MonitorInfo {
            id: m["id"].as_u64().unwrap() as u32,
            name: m["name"].as_str().unwrap().to_owned(),
            x: m["x"].as_f64().unwrap(),
            y: m["y"].as_f64().unwrap(),
            width: m["width"].as_f64().unwrap(),
            height: m["height"].as_f64().unwrap(),
            scale: m["scale"].as_f64().unwrap(),
        })
        .collect()
}

fn monitor_named(ipc: &HyprIpc, name: &str) -> MonitorInfo {
    monitors(ipc)
        .into_iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("no monitor {name}"))
}

/// Creating or removing an output makes the parent compositor re-tile the nest's windows, which
/// resizes the outputs for a moment: wait until the monitor list holds still for half a second.
fn settle_outputs(ipc: &HyprIpc) {
    let snapshot = |ipc: &HyprIpc| {
        monitors(ipc)
            .into_iter()
            .map(|m| {
                (
                    m.name,
                    m.x as i64,
                    m.y as i64,
                    m.width as i64,
                    m.height as i64,
                )
            })
            .collect::<Vec<_>>()
    };
    // A nest that has only its fallback monitor is still waiting for the parent compositor to
    // give its window a size, which a busy desktop can take a while to do.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = snapshot(ipc);
    let mut since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let now = snapshot(ipc);
        // A fresh nest's output has no size at all for a moment: that is not settled.
        let sized = !now.is_empty() && now.iter().all(|m| m.3 > 0 && m.4 > 0);
        if now != last || !sized {
            last = now;
            since = Instant::now();
        } else if since.elapsed() >= Duration::from_millis(500) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the outputs never settled: {last:?}"
        );
    }
}

/// The monitor rule that keeps `m` at its mode and scale and puts it at layout `(x, y)`.
fn place(m: &MonitorInfo, x: f64, y: f64) -> String {
    format!(
        "hl.monitor({{ output = \"{}\", mode = \"{}x{}@60\", position = \"{}x{}\", scale = {} }})",
        m.name, m.width, m.height, x, y, m.scale
    )
}

/// A second output of the nest (`output create wayland`, P2), removed on drop.
struct SecondOutput {
    ipc: HyprIpc,
    name: String,
    display: DisplayId,
}
impl SecondOutput {
    /// `None`, with the reason printed, if this nest can't make one: the P2 tests then skip.
    fn new(ipc: &HyprIpc, name: &str) -> Option<Self> {
        match ipc.request(&format!("output create wayland {name}")) {
            Ok(reply) if reply.trim() == "ok" => (),
            other => {
                eprintln!("skipped: no second output: {other:?}");
                return None;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((_, id)) = ipc
                .monitor_ids()
                .unwrap()
                .into_iter()
                .find(|(n, _)| n == name)
            {
                let output = Self {
                    ipc: ipc.clone(),
                    name: name.to_owned(),
                    display: DisplayId(id),
                };
                output.place_beside_the_others();
                return Some(output);
            }
            if Instant::now() >= deadline {
                let _ = ipc.request(&format!("output remove {name}"));
                eprintln!("skipped: no second output: {name} never appeared");
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    /// Hyprland places a new output at an "auto" position that is a fixed offset (the width the
    /// nest was configured with), not the end of the layout: once the nest's window is wider than
    /// that, the outputs overlap and a point belongs to either. Put this one right of the others,
    /// where the tests expect it, after the parent compositor has finished re-tiling.
    fn place_beside_the_others(&self) {
        settle_outputs(&self.ipc);
        let all = monitors(&self.ipc);
        let Some(me) = all.iter().find(|m| m.name == self.name) else {
            return;
        };
        let edge = all
            .iter()
            .filter(|m| m.name != self.name)
            .map(|m| m.x + m.width / m.scale)
            .fold(0.0, f64::max);
        if me.x >= edge {
            return;
        }
        // The nest's config positions every output "auto": an explicit position for one makes
        // Hyprland lay the others out again. Pin the others where they are first.
        for m in all.iter().filter(|m| m.name != self.name) {
            self.ipc.eval(&place(m, m.x, m.y)).unwrap();
        }
        self.ipc.eval(&place(me, edge, 0.0)).unwrap();
        settle_outputs(&self.ipc);
    }
    /// Remove the output and wait until the compositor no longer lists it.
    fn remove(&self) {
        let _ = self.ipc.request(&format!("output remove {}", self.name));
        let deadline = Instant::now() + Duration::from_secs(10);
        while self
            .ipc
            .monitor_ids()
            .unwrap()
            .iter()
            .any(|(n, _)| *n == self.name)
        {
            assert!(Instant::now() < deadline, "{} was never removed", self.name);
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for SecondOutput {
    fn drop(&mut self) {
        let _ = self.ipc.request(&format!("output remove {}", self.name));
    }
}

/// Output creation and removal re-tile the nest and change the topology every other test sees,
/// so the tests that need a second output run again in a nest of their own (as
/// `compositor_going_away` does). Returns true in that nest: run the test body there.
fn dedicated(test: &str) -> bool {
    if !nested() {
        return false;
    }
    if std::env::var("CROSSPANE_CAPTURE_CHILD").as_deref() == Ok(test) {
        // A fresh nest's output is still being resized by the parent compositor; strips made
        // before that settles would be placed on the old size.
        settle_outputs(&ipc());
        return true;
    }
    let _exclusive = lock_file(
        "crosspane-capture-topology.lock",
        rustix::fs::FlockOperation::LockExclusive,
    );
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/hypr-nested.sh");
    let name = format!("wp243d-{test}-{}", std::process::id());
    struct Stop(PathBuf, String);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = Command::new(&self.0)
                .args(["stop", "--name", &self.1])
                .status();
        }
    }
    let _stop = Stop(script.clone(), name.clone());
    let parent = std::env::var("CROSSPANE_PARENT_WAYLAND_DISPLAY")
        .unwrap_or_else(|_| std::env::var("WAYLAND_DISPLAY").unwrap());
    assert!(
        Command::new(&script)
            .args(["start", "--name", &name])
            .env("WAYLAND_DISPLAY", parent)
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("WAYLAND_SOCKET")
            .status()
            .unwrap()
            .success()
    );
    let exports = Command::new(&script)
        .args(["env", "--name", &name])
        .output()
        .unwrap();
    assert!(exports.status.success());
    let mut signature = String::new();
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", test, "--nocapture"])
        .env("CROSSPANE_CAPTURE_CHILD", test)
        .env_remove("WAYLAND_SOCKET");
    for line in String::from_utf8(exports.stdout).unwrap().lines() {
        if line == "unset WAYLAND_SOCKET" {
            continue; // `env_remove` above does the same for the child.
        }
        let (key, value) = line
            .strip_prefix("export ")
            .unwrap()
            .split_once('=')
            .unwrap();
        assert!(matches!(
            key,
            "WAYLAND_DISPLAY" | "HYPRLAND_INSTANCE_SIGNATURE" | "CROSSPANE_NESTED_HYPR"
        ));
        assert!(
            value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        );
        child.env(key, value);
        if key == "HYPRLAND_INSTANCE_SIGNATURE" {
            signature = value.to_owned();
        }
    }
    let result = child.output().unwrap();
    // The nest is going away: so is the lock file that `serialize` made for it.
    let _ = std::fs::remove_file(
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
            .join(format!("crosspane-capture-{signature}.lock")),
    );
    // The child's measurements are the point of some tests: pass them on.
    eprint!("{}", String::from_utf8_lossy(&result.stderr));
    assert!(
        result.status.success(),
        "{test} failed in its own nest:\n{}",
        String::from_utf8_lossy(&result.stdout)
    );
    false
}

/// A capture backend with nothing subscribed: enough for `set_portals` and plain warps.
fn bare_backend() -> HyprlandCapture {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    // Verified before the backend connects, and connected to exactly the verified endpoint.
    HyprlandCapture::new_for_nest_test(gate, &require_nest()).unwrap()
}

/// `set_portals` with the test hooks. The endpoints are verified afresh first, and the backend
/// itself refuses the hook unless it was built from a proof and the environment still names
/// exactly the endpoint that proof was made for.
fn portals_with(
    capture: &mut HyprlandCapture,
    portals: &[CapturePortal],
    budget: Duration,
    hooks: PortalsTestHooks,
) -> Result<(), PlatformError> {
    require_nest();
    capture.set_portals_for_test(portals, budget, hooks)
}

/// Why the backend ended captures so far: its own decisions against the compositor's doing.
fn end_causes(capture: &HyprlandCapture) -> Vec<String> {
    require_nest();
    capture.end_causes_for_test().unwrap()
}

/// Install `portals`, retrying while the backend refuses (a topology change takes a moment to
/// reach it). Every refusal must be a rejection: the previous set and any capture stay intact.
fn install(capture: &mut HyprlandCapture, portals: &[CapturePortal]) -> Duration {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let started = Instant::now();
        match capture.set_portals(portals) {
            Ok(()) => return started.elapsed(),
            Err(e) => {
                assert_eq!(
                    set_portals_failure(&e),
                    SetPortalsFailure::Rejected,
                    "a refusal must leave the previous set intact: {e}"
                );
                assert!(Instant::now() < deadline, "set_portals kept failing: {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// A plain warp (no capture active), retrying while the backend has not heard of the output yet.
fn warp(capture: &mut HyprlandCapture, display: DisplayId, x: f64, y: f64) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match capture.end(Some((display, PointDevice::new(x, y)))) {
            Ok(()) => return,
            Err(PlatformError::NotFound) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => panic!("warp to {display:?} failed: {e}"),
        }
    }
}

/// The compositor's cursor is `p` on `m` (layout position = the output's position + p / scale),
/// both by `cursorpos` and by the library's read-back helper.
fn assert_cursor(ipc: &HyprIpc, m: &MonitorInfo, p: PointDevice) {
    let raw = ipc.json("cursorpos").unwrap();
    let (rx, ry) = (raw["x"].as_f64().unwrap(), raw["y"].as_f64().unwrap());
    let (ex, ey) = (m.x + p.x / m.scale, m.y + p.y / m.scale);
    assert!(
        (rx - ex).abs() <= 1.0 && (ry - ey).abs() <= 1.0,
        "cursorpos is ({rx}, {ry}), expected ({ex}, {ey}) for {p:?} on {m:?}"
    );
    let (display, at) = cursor_position(ipc).unwrap();
    assert_eq!(
        display,
        DisplayId(m.id),
        "the helper put the cursor elsewhere"
    );
    assert!(
        (at.x - p.x).abs() <= 2.0 && (at.y - p.y).abs() <= 2.0,
        "the helper reads {at:?}, the warp was to {p:?}"
    );
}

/// The capture is live and still forwards motion, and nothing ended it on the way.
fn assert_capture_flows(f: &mut Fixture) {
    f.driver.motion(1.0, 0.0);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match f
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(CaptureEvent::Motion { .. }) => return,
            Ok(CaptureEvent::Ended { id, reason }) => panic!("capture {id:?} ended: {reason:?}"),
            Ok(_) => (),
            Err(e) => panic!("no motion reached the capture: {e}"),
        }
    }
}

fn portal_on(id: u32, edge: Edge, f: &Fixture) -> CapturePortal {
    let (w, h) = (
        f64::from(f.driver.state.width),
        f64::from(f.driver.state.height),
    );
    let length = if matches!(edge, Edge::Left | Edge::Right) {
        h
    } else {
        w
    };
    CapturePortal {
        id: PortalId(id),
        edge,
        from: length / 4.0,
        to: length * 3.0 / 4.0,
        ..f.portal
    }
}

#[test]
fn set_portals_keeps_unchanged_strips() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    let ipc = ipc();
    let (w, h) = (f.driver.state.width, f.driver.state.height);
    let right = f.portal;
    let left = portal_on(2, Edge::Left, &f);
    let top = portal_on(3, Edge::Top, &f);
    let fds_one = settled_fd_count();
    f.capture.set_portals(&[right, left, top]).unwrap();
    let first = wait_strips(&ipc, 3);
    let fds = settled_fd_count();
    eprintln!(
        "compositor descriptors: {fds_one} with one strip, {fds} with three (two new strips cost {})",
        fds as i64 - fds_one as i64
    );
    // The same set, and the same set in another order: no request reaches the compositor.
    for set in [[right, left, top], [top, right, left], [left, top, right]] {
        f.capture.set_portals(&set).unwrap();
    }
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(strips(&ipc), first, "an identical set changed the strips");
    assert_eq!(
        settled_fd_count(),
        fds,
        "an identical set cost the compositor descriptors"
    );
    // One added (bottom), one removed (top), one resized (left): only those change.
    let bottom = portal_on(4, Edge::Bottom, &f);
    let left_resized = CapturePortal {
        to: left.to - 24.0,
        ..left
    };
    f.capture
        .set_portals(&[right, left_resized, bottom])
        .unwrap();
    let second = wait_strips(&ipc, 3);
    let address =
        |listed: &[StripInfo], edge: Edge| on_edge(listed, edge, w, h).map(|s| s.address.clone());
    assert_eq!(
        address(&second, Edge::Right),
        address(&first, Edge::Right),
        "the unchanged strip was recreated"
    );
    assert_ne!(
        address(&second, Edge::Left),
        address(&first, Edge::Left),
        "a resized portal kept its old strip"
    );
    assert!(address(&first, Edge::Bottom).is_none() && address(&second, Edge::Bottom).is_some());
    assert!(address(&first, Edge::Top).is_some() && address(&second, Edge::Top).is_none());
    // The strip that was resized has the new extent; the kept one is untouched.
    let resized = on_edge(&second, Edge::Left, w, h).unwrap();
    let old = on_edge(&first, Edge::Left, w, h).unwrap();
    assert_eq!(resized.h, old.h - 24);
    // Removing everything is a plain removal.
    f.capture.set_portals(&[]).unwrap();
    wait_strips(&ipc, 0);
}

#[test]
fn capture_survives_portal_changes() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    f.enter();
    f.start(CaptureId(1));
    let ipc = ipc();
    let right = f.portal;
    let left = portal_on(2, Edge::Left, &f);
    let top = portal_on(3, Edge::Top, &f);
    let before = wait_strips(&ipc, 1);
    // Add, then add another, resize one, remove them: the captured strip is never touched.
    f.capture.set_portals(&[right, left]).unwrap();
    assert_capture_flows(&mut f);
    f.capture.set_portals(&[right, left, top]).unwrap();
    assert_capture_flows(&mut f);
    f.capture
        .set_portals(&[
            right,
            CapturePortal {
                to: left.to - 20.0,
                ..left
            },
            top,
        ])
        .unwrap();
    assert_capture_flows(&mut f);
    f.capture.set_portals(&[top, right]).unwrap();
    assert_capture_flows(&mut f);
    f.capture.set_portals(&[right]).unwrap();
    assert_capture_flows(&mut f);
    // An identical set during a capture is a no-op.
    f.capture.set_portals(&[right]).unwrap();
    assert_capture_flows(&mut f);
    let after = wait_strips(&ipc, 1);
    assert_eq!(before, after, "the captured strip was replaced");
    f.capture.end(None).unwrap();
    f.ended(CaptureId(1), EndReason::Requested);
    assert!(
        f.events
            .try_iter()
            .all(|e| !matches!(e, CaptureEvent::Ended { .. })),
        "a second Ended arrived"
    );
}

#[test]
fn changed_active_strip_ends_capture_lost() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    let left = portal_on(2, Edge::Left, &f);
    f.capture.set_portals(&[f.portal, left]).unwrap();
    // Resized: the captured strip is a new portal, so the capture ends and the other strip stays.
    f.enter();
    f.start(CaptureId(1));
    f.capture
        .set_portals(&[
            CapturePortal {
                to: f.portal.to - 12.0,
                ..f.portal
            },
            left,
        ])
        .unwrap();
    f.ended(CaptureId(1), EndReason::Lost);
    // Removed from the set: the same.
    f.portal.to -= 12.0;
    f.enter();
    f.start(CaptureId(2));
    f.capture.set_portals(&[left]).unwrap();
    f.ended(CaptureId(2), EndReason::Lost);
    // Moved to another edge under the same id: a new strip, and the capture on the old one is lost.
    f.capture.set_portals(&[f.portal, left]).unwrap();
    f.enter();
    f.start(CaptureId(3));
    f.capture
        .set_portals(&[
            CapturePortal {
                edge: Edge::Top,
                from: 10.0,
                to: 100.0,
                ..f.portal
            },
            left,
        ])
        .unwrap();
    f.ended(CaptureId(3), EndReason::Lost);
}

#[test]
fn set_portals_result_means_installed() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    let ipc = ipc();
    let (w, h) = (f.driver.state.width, f.driver.state.height);
    // `Fixture::new` returned from an `Ok` `set_portals`: its strip is listed already.
    let kept = strips(&ipc);
    assert_eq!(kept.len(), 1, "{kept:?}");
    let extra = portal_on(2, Edge::Left, &f);
    let third = portal_on(3, Edge::Top, &f);
    // Refused sets are refused whole: nothing of them is installed, the previous set stays mapped
    // and the error says it was rejected (so the capture, if any, is known to be intact).
    let refused = [
        CapturePortal {
            display: DisplayId(9999),
            ..third
        },
        CapturePortal {
            to: f64::from(w) + 50.0,
            ..third
        },
        CapturePortal {
            from: 10.0,
            to: 5.0,
            ..third
        },
        CapturePortal {
            to: f64::NAN,
            ..third
        },
        CapturePortal {
            id: f.portal.id,
            ..third
        },
    ];
    for bad in refused {
        let e = f.capture.set_portals(&[f.portal, extra, bad]).unwrap_err();
        assert_eq!(
            set_portals_failure(&e),
            SetPortalsFailure::Rejected,
            "{bad:?}: {e}"
        );
        assert_eq!(strips(&ipc), kept, "{bad:?}: the previous set changed");
    }
    // Ok means every requested strip is mapped, at the requested place (a strip covers whole
    // layout pixels around the portal's device-pixel stretch).
    f.capture.set_portals(&[f.portal, extra]).unwrap();
    // At `Ok` the complete set is listed already, with no polling (that the call waits for the
    // acknowledgements is `installation_is_confirmed_before_return`).
    let both = strips(&ipc);
    assert_eq!(both.len(), 2, "{both:?}");
    let covers = |p: &CapturePortal| (p.from.floor() as i64, (p.to.ceil() - p.from.floor()) as i64);
    let right = on_edge(&both, Edge::Right, w, h).unwrap();
    assert_eq!((right.y, right.h), covers(&f.portal));
    let left = on_edge(&both, Edge::Left, w, h).unwrap();
    assert_eq!((left.y, left.h), covers(&extra));
    // A deadline that cannot be met: the caller gives up and the backend aborts. The error is the
    // caller's timeout (the previous set's fate is unknown), and the backend rebuilds the
    // previous set by itself; the new strip is never installed.
    let e = portals_with(
        &mut f.capture,
        &[f.portal, extra, third],
        Duration::ZERO,
        PortalsTestHooks::default(),
    )
    .unwrap_err();
    assert!(matches!(e, PlatformError::Timeout), "{e}");
    assert_eq!(set_portals_failure(&e), SetPortalsFailure::Uncertain);
    std::thread::sleep(Duration::from_millis(400));
    let rebuilt = wait_strips(&ipc, 2);
    assert!(on_edge(&rebuilt, Edge::Top, w, h).is_none());
    assert_eq!(
        (
            on_edge(&rebuilt, Edge::Right, w, h).map(|s| (s.y, s.h)),
            on_edge(&rebuilt, Edge::Left, w, h).map(|s| (s.y, s.h))
        ),
        (Some(covers(&f.portal)), Some(covers(&extra))),
        "the previous set is not what was rebuilt"
    );
    // And the retry the engine would make succeeds.
    install(&mut f.capture, &[f.portal, extra, third]);
    wait_strips(&ipc, 3);
}

#[test]
fn set_portals_error_classes_during_capture() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    let ipc = ipc();
    f.enter();
    f.start(CaptureId(1));
    let before = wait_strips(&ipc, 1);
    let extra = portal_on(2, Edge::Left, &f);
    // A refresh that misses its bound is the worker's own report: a rejection. The previous set
    // is in force and the capture goes on.
    let e = portals_with(
        &mut f.capture,
        &[f.portal, extra],
        Duration::from_millis(40),
        PortalsTestHooks {
            refresh_bound: Some(Duration::ZERO),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(set_portals_failure(&e), SetPortalsFailure::Rejected, "{e}");
    assert!(
        !matches!(e, PlatformError::Timeout),
        "a worker-reported timeout must not look like the caller's"
    );
    assert_capture_flows(&mut f);
    assert_eq!(strips(&ipc), before);
    // A refresh whose list is read in time but parsed late is the same rejection, and its list is
    // not published (unit test `a_late_refresh_publishes_nothing`).
    let e = portals_with(
        &mut f.capture,
        &[f.portal, extra],
        Duration::from_millis(120),
        PortalsTestHooks {
            refresh_bound: Some(Duration::from_millis(20)),
            refresh_stall: Duration::from_millis(40),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(set_portals_failure(&e), SetPortalsFailure::Rejected, "{e}");
    assert_capture_flows(&mut f);
    assert_eq!(strips(&ipc), before);
    // The same set installs once the refresh has its normal bound.
    f.capture.set_portals(&[f.portal, extra]).unwrap();
    assert_capture_flows(&mut f);
    // The caller's own timeout aborts the backend: the capture's fate is unknown (`Uncertain`)
    // and it ends `Aborted`.
    let e = portals_with(
        &mut f.capture,
        &[f.portal],
        Duration::ZERO,
        PortalsTestHooks::default(),
    )
    .unwrap_err();
    assert!(matches!(e, PlatformError::Timeout), "{e}");
    assert_eq!(set_portals_failure(&e), SetPortalsFailure::Uncertain);
    f.ended(CaptureId(1), EndReason::Aborted);
    // The backend recovers on its own with the last installed set.
    std::thread::sleep(Duration::from_millis(400));
    wait_strips(&ipc, 2);
}

/// Install `portals`, retrying on any failure: right after a connection was lost the backend is
/// rebuilding, and a command may meet the old, dying connection first (the engine retries too).
fn install_any(capture: &mut HyprlandCapture, portals: &[CapturePortal]) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match capture.set_portals(portals) {
            Ok(()) => return,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "no portal command succeeds after the connection was lost: {e}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Run `set_portals` with its acknowledgements held back, interfere with the compositor while the
/// strips are being prepared, and return what the call returned.
fn while_preparing(
    capture: &mut HyprlandCapture,
    portals: &[CapturePortal],
    hooks: PortalsTestHooks,
    interfere: impl FnOnce(),
) -> Result<(), PlatformError> {
    std::thread::scope(|scope| {
        let call =
            scope.spawn(move || portals_with(capture, portals, Duration::from_secs(5), hooks));
        // Let the strips be created and the acknowledgements be held, then interfere.
        std::thread::sleep(Duration::from_millis(120));
        assert!(
            !call.is_finished(),
            "the call ended before the interference"
        );
        interfere();
        call.join().unwrap()
    })
}

/// The test hooks are authorised by how a backend was built, not by its environment. A backend
/// made by the production constructor (no proof) refuses every public hook, here even though the
/// process runs in a verified nest with `CROSSPANE_NESTED_HYPR=1` and the endpoint variables name
/// the very compositor the backend is connected to. (It connects to the verified nest, never to
/// the live session; the library's unit tests refuse the same hooks on a backend that was never
/// connected at all.) Nothing a refused hook asked for happens.
#[test]
fn hooks_refuse_on_a_production_backend() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    let mut production = HyprlandCapture::new(gate).unwrap();
    let first = monitors(&ipc)[0].clone();
    let portal = CapturePortal {
        id: PortalId(1),
        display: DisplayId(first.id),
        edge: Edge::Right,
        from: 4.0,
        to: 60.0,
    };
    production.set_portals(&[portal]).unwrap();
    let before = wait_strips(&ipc, 1);
    let results = [
        production.inject_worker_error_for_test(),
        production.set_portals_for_test(
            &[],
            Duration::from_millis(200),
            PortalsTestHooks::default(),
        ),
        production.end_causes_for_test().map(|_| ()),
        production.output_removal_for_test(&first.name),
    ];
    for result in results {
        assert!(
            matches!(result, Err(PlatformError::Unsupported(_))),
            "a hook acted on a production backend: {result:?}"
        );
    }
    // The strip is still there, the worker is not failing, and the backend still works.
    assert_eq!(strips(&ipc), before);
    production.set_portals(&[portal]).unwrap();
    assert_eq!(strips(&ipc), before);
    production.set_portals(&[]).unwrap();
    wait_strips(&ipc, 0);
}

/// A worker that is asked late rejects at once instead of spending the caller's time (§3.6.2,
/// B1): with only 3 ms of the budget left, which is less than the reply reserve, the answer is a
/// rejection that arrives *before* the caller's deadline, so the caller does not time out and
/// abort the capture that the rejection leaves intact.
#[test]
fn reserve_exhausted_rejects_at_once() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    let ipc = ipc();
    let extra = portal_on(2, Edge::Left, &f);
    let budget = Duration::from_millis(200);
    let hooks = PortalsTestHooks {
        leave: Some(Duration::from_millis(3)),
        ..Default::default()
    };
    for attempt in 1..=5 {
        let id = CaptureId(attempt);
        f.enter();
        f.start(id);
        let before = strips(&ipc);
        let started = Instant::now();
        match portals_with(&mut f.capture, &[f.portal, extra], budget, hooks) {
            Err(e) if set_portals_failure(&e) == SetPortalsFailure::Rejected => {
                assert!(
                    started.elapsed() < budget,
                    "the rejection came after the caller's deadline: {:?}",
                    started.elapsed()
                );
                assert!(e.to_string().contains("no time left"), "{e}");
                // The capture is intact, and so is the set.
                assert_capture_flows(&mut f);
                assert_eq!(strips(&ipc), before);
                f.capture.end(None).unwrap();
                f.ended(id, EndReason::Requested);
                return;
            }
            // The worker overslept the last milliseconds (scheduling): the caller timed out and
            // aborted. Recover and try again.
            Err(PlatformError::Timeout) => {
                f.ended(id, EndReason::Aborted);
                std::thread::sleep(Duration::from_millis(400));
                wait_strips(&ipc, 1);
            }
            other => panic!("neither a rejection nor the caller's timeout: {other:?}"),
        }
    }
    panic!("the worker never answered within the reserve in 5 attempts");
}

/// `Ok` is the installation's truth: the call does not return before the new strips are
/// configured and the roundtrip that follows their buffers has been acknowledged, and at `Ok` the
/// whole set is mapped and buffered. A controlled delay of those acknowledgements shows the call
/// pending; at `Ok` the compositor lists the complete set at once (no polling) and the pointer
/// reaches every new strip, which only a mapped, buffered surface allows.
#[test]
fn installation_is_confirmed_before_return() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    let ipc = ipc();
    let (w, h) = (f.driver.state.width, f.driver.state.height);
    let portal = f.portal;
    let (extra, third) = (portal_on(2, Edge::Left, &f), portal_on(3, Edge::Top, &f));
    assert_eq!(strips(&ipc).len(), 1);
    let hold = Duration::from_millis(150);
    let hooks = PortalsTestHooks {
        ack_delay: hold,
        ..Default::default()
    };
    let started = Instant::now();
    let result = std::thread::scope(|scope| {
        let capture = &mut f.capture;
        let call = scope.spawn(move || {
            portals_with(
                capture,
                &[portal, extra, third],
                Duration::from_secs(3),
                hooks,
            )
        });
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            !call.is_finished(),
            "set_portals returned before the new strips were configured"
        );
        call.join().unwrap()
    });
    result.unwrap();
    // Two acknowledgements are held `hold` each, one after the other: the configures of the new
    // strips, then the roundtrip that follows their buffers.
    assert!(
        started.elapsed() >= hold * 2,
        "set_portals returned after {:?}, before the held configure and roundtrip acknowledgements \
         ({hold:?} each)",
        started.elapsed()
    );
    // At `Ok`: the complete set, listed at once.
    let listed = strips(&ipc);
    assert_eq!(listed.len(), 3, "{listed:?}");
    let covers = |p: &CapturePortal| (p.from.floor() as i64, (p.to.ceil() - p.from.floor()) as i64);
    assert_eq!(
        on_edge(&listed, Edge::Right, w, h).map(|s| (s.y, s.h)),
        Some(covers(&portal))
    );
    assert_eq!(
        on_edge(&listed, Edge::Left, w, h).map(|s| (s.y, s.h)),
        Some(covers(&extra))
    );
    assert_eq!(
        on_edge(&listed, Edge::Top, w, h).map(|s| s.x),
        Some(covers(&third).0)
    );
    // Mapped and buffered: the pointer is on each new strip at once.
    while f.events.try_recv().is_ok() {}
    for (edge, id) in [(Edge::Left, 2), (Edge::Top, 3)] {
        f.driver.absolute(edge, false);
        f.driver.absolute(edge, true);
        f.wait(
            |e| matches!(e, CaptureEvent::EdgePressed { portal, .. } if *portal == PortalId(id)),
        );
    }
    // Held longer than the call may wait: the worker answers at its own deadline with a
    // rejection, nothing of the new set is installed, and the previous set is untouched. (Only
    // the cleanup of the unfinished strip is asynchronous, so only that is polled.)
    let bottom = portal_on(4, Edge::Bottom, &f);
    let started = Instant::now();
    let e = portals_with(
        &mut f.capture,
        &[portal, extra, third, bottom],
        Duration::from_millis(400),
        PortalsTestHooks {
            ack_delay: Duration::from_secs(2),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(set_portals_failure(&e), SetPortalsFailure::Rejected, "{e}");
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "the call gave up after {:?}, before its own deadline",
        started.elapsed()
    );
    assert_eq!(wait_strips(&ipc, 3), listed);
}

/// What a rejection may claim. A rejection says the previous set and any capture are intact, so a
/// loss that dispatch causes *while the command runs* (an installed strip destroyed, a capture
/// ended by the compositor) must make the error uncertain; a loss of only a strip that was still
/// being made must not.
#[test]
fn loss_during_preparation_is_uncertain() {
    if !dedicated("loss_during_preparation_is_uncertain") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    let (Some(b), Some(c)) = (
        SecondOutput::new(&ipc, "wp243d-prep-b"),
        SecondOutput::new(&ipc, "wp243d-prep-c"),
    ) else {
        return;
    };
    settle_outputs(&ipc);
    let mut f = Fixture::new();
    f.portal = CapturePortal {
        from: 4.0,
        to: 60.0,
        ..f.portal
    };
    f.entry_y = Some(30);
    let portal = f.portal;
    let on = |id: u32, display: DisplayId| CapturePortal {
        id: PortalId(id),
        display,
        ..portal
    };
    let (pb, pc) = (on(2, b.display), on(3, c.display));
    install(&mut f.capture, &[portal, pb]);
    wait_strips(&ipc, 2);
    let hooks = PortalsTestHooks {
        ack_delay: Duration::from_millis(400),
        ..Default::default()
    };

    // 1. Only a strip that is being made loses its output (C goes while C's strip is prepared).
    // Nothing installed was touched, so the rejection stays true.
    let e = while_preparing(&mut f.capture, &[portal, pb, pc], hooks, || c.remove()).unwrap_err();
    assert_eq!(set_portals_failure(&e), SetPortalsFailure::Rejected, "{e}");
    wait_strips(&ipc, 2);

    // 2. An installed strip loses its output while a capture is on another one (B goes while a
    // new strip on A is prepared): the strip is gone and the compositor ends the capture. The
    // `NotFound` that follows must not claim the previous set and the capture survived.
    settle_outputs(&ipc);
    f.capture.set_portals(&[]).unwrap();
    install(&mut f.capture, &[portal, pb]);
    wait_strips(&ipc, 2);
    f.enter();
    f.start(CaptureId(1));
    let pd = CapturePortal {
        id: PortalId(4),
        edge: Edge::Left,
        from: 4.0,
        to: 40.0,
        ..portal
    };
    let e = while_preparing(&mut f.capture, &[portal, pb, pd], hooks, || b.remove()).unwrap_err();
    assert_eq!(set_portals_failure(&e), SetPortalsFailure::Uncertain, "{e}");
    let lost = lost_within(&f, CaptureId(1), Duration::from_secs(1));
    let causes = end_causes(&f.capture);
    eprintln!(
        "an installed strip lost during preparation: Uncertain ({e}); compositor-induced loss of \
         the capture: {lost}, causes {causes:?}"
    );
    assert!(
        !causes
            .iter()
            .any(|c| matches!(c.as_str(), "OutputRemoved" | "StripReplaced")),
        "{causes:?}"
    );
}

/// A removed output must not keep the backend from installing anything after its connection is
/// lost. The worker used to rebuild its cached set (which still named the removed output) before
/// any new command, and that failed forever: no replacement, not even an empty set, was ever
/// tried, and neither was the background recovery.
#[test]
fn removed_output_does_not_block_reinstall_after_shutdown() {
    if !dedicated("removed_output_does_not_block_reinstall_after_shutdown") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    let Some(b) = SecondOutput::new(&ipc, "wp243d-reinstall-b") else {
        return;
    };
    settle_outputs(&ipc);
    let mut f = Fixture::new();
    f.portal = CapturePortal {
        from: 4.0,
        to: 60.0,
        ..f.portal
    };
    f.entry_y = Some(30);
    let portal = f.portal;
    let on_b = |display: DisplayId| CapturePortal {
        id: PortalId(2),
        display,
        ..portal
    };
    install(&mut f.capture, &[portal, on_b(b.display)]);
    wait_strips(&ipc, 2);

    // B goes, then the connection: the cached set is [A, B]. A command that installs A alone
    // must succeed on the fresh connection, and so must an empty set after another shutdown.
    b.remove();
    f.capture.abort_handle().abort();
    install_any(&mut f.capture, &[portal]);
    settle_outputs(&ipc);
    f.capture.abort_handle().abort();
    install_any(&mut f.capture, &[]);
    install_any(&mut f.capture, &[portal]);
    f.enter();
    f.start(CaptureId(1));
    f.capture.end(None).unwrap();
    f.ended(CaptureId(1), EndReason::Requested);

    // The same without any command: background recovery rebuilds what it can place. A second
    // output comes and goes with a strip on it, the connection is lost, nothing is sent, and the
    // strip on A is back (the pointer reaches it: `enter` waits for exactly that).
    let Some(c) = SecondOutput::new(&ipc, "wp243d-reinstall-c") else {
        return;
    };
    settle_outputs(&ipc);
    install(&mut f.capture, &[portal, on_b(c.display)]);
    wait_strips(&ipc, 2);
    c.remove();
    f.capture.abort_handle().abort();
    settle_outputs(&ipc);
    f.enter();
    f.start(CaptureId(2));
    f.capture.end(None).unwrap();
    f.ended(CaptureId(2), EndReason::Requested);
}

#[test]
fn topology_refresh_during_capture() {
    if !dedicated("topology_refresh_during_capture") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    let mut f = Fixture::new();
    // A short strip near the top stays valid when the first output shrinks (the parent compositor
    // re-tiles the nest's windows once the second output's window appears).
    f.portal = CapturePortal {
        from: 4.0,
        to: 60.0,
        ..f.portal
    };
    f.entry_y = Some(30);
    f.capture.set_portals(&[f.portal]).unwrap();
    f.enter();
    f.start(CaptureId(1));
    let Some(second) = SecondOutput::new(&ipc, "wp243d-topology") else {
        f.capture.end(None).unwrap();
        return;
    };
    settle_outputs(&ipc);
    assert_capture_flows(&mut f);
    // The next set that names the new output is accepted without ending the capture.
    let on_second = CapturePortal {
        id: PortalId(2),
        display: second.display,
        edge: Edge::Right,
        from: 4.0,
        to: 60.0,
    };
    let took = install(&mut f.capture, &[f.portal, on_second]);
    eprintln!(
        "U8: set_portals with a monitor refresh during a capture took {took:?} (budget 40 ms)"
    );
    let listed = wait_strips(&ipc, 2);
    assert!(listed.iter().any(|s| s.monitor == second.name));
    assert!(listed.iter().any(|s| s.monitor != second.name));
    assert_capture_flows(&mut f);
    // The backend knows the new output well enough to end the capture with a warp onto it (the
    // home flow's twin output may well appear while the controller is capturing).
    let landing = PointDevice::new(30.0, 20.0);
    f.capture.end(Some((second.display, landing))).unwrap();
    f.ended(CaptureId(1), EndReason::Requested);
    assert_cursor(&ipc, &monitor_named(&ipc, &second.name), landing);
}

/// Hyprland does not always move an edge-anchored layer surface when its output is resized after
/// the surface was made (a nested output resized by its parent window leaves a right-edge strip
/// at the old edge). A strip that is not being captured on is therefore replaced when its
/// output's size is no longer the one it was made for, even though its portal is unchanged.
#[test]
fn strip_is_replaced_after_its_output_is_resized() {
    if !dedicated("strip_is_replaced_after_its_output_is_resized") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    let mut f = Fixture::new();
    f.portal = CapturePortal {
        from: 4.0,
        to: 60.0,
        ..f.portal
    };
    f.capture.set_portals(&[f.portal]).unwrap();
    let before = wait_strips(&ipc, 1);
    let first = monitors(&ipc)[0].clone();
    // The parent compositor re-tiles the nest's windows once the second output's window appears.
    let Some(second) = SecondOutput::new(&ipc, "wp243d-resize") else {
        return;
    };
    settle_outputs(&ipc);
    let resized = monitor_named(&ipc, &first.name);
    if (resized.width, resized.height) == (first.width, first.height) {
        eprintln!("skipped: the first output kept its size when the second one appeared");
        return;
    }
    // The same portal, asked for again: the strip is made anew for the output as it is now.
    f.capture.set_portals(&[f.portal]).unwrap();
    let after = wait_strips(&ipc, 1);
    assert_ne!(
        before[0].address, after[0].address,
        "the stale strip was kept"
    );
    assert_eq!(
        after[0].x,
        resized.width as i64 - 1,
        "{after:?} on {resized:?}"
    );
    // Asked for once more with nothing resized meanwhile, it is kept.
    f.capture.set_portals(&[f.portal]).unwrap();
    assert_eq!(wait_strips(&ipc, 1), after);
    drop(second);
}

/// Give `m` another scale in place (same mode, same position).
fn set_scale(ipc: &HyprIpc, m: &MonitorInfo, scale: f64) {
    ipc.eval(&place(&MonitorInfo { scale, ..m.clone() }, m.x, m.y))
        .unwrap();
    settle_outputs(ipc);
}

/// A changed scale on a strip's output makes the strip a new one (its layout size and margin
/// derive from the scale), and ends a capture on it `Lost` at the next `set_portals` (§3.6.2).
#[test]
fn scale_change_replaces_strips_and_ends_a_capture_on_them() {
    if !dedicated("scale_change_replaces_strips_and_ends_a_capture_on_them") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    let mut f = Fixture::new();
    f.portal = CapturePortal {
        from: 8.0,
        to: 80.0,
        ..f.portal
    };
    f.entry_y = Some(30);
    f.capture.set_portals(&[f.portal]).unwrap();
    let before = wait_strips(&ipc, 1);
    let first = monitors(&ipc)[0].clone();
    assert_eq!((before[0].y, before[0].h), (8, 72));
    // Not captured: the same portal on an output of another scale is a new strip, made for it.
    set_scale(&ipc, &first, 2.0);
    f.capture.set_portals(&[f.portal]).unwrap();
    let after = wait_strips(&ipc, 1);
    assert_ne!(
        before[0].address, after[0].address,
        "the strip kept its old scale"
    );
    assert_eq!((after[0].y, after[0].h), (4, 36), "{after:?}");
    // Back to scale one, then capture on the strip and change the scale under it.
    set_scale(&ipc, &first, 1.0);
    f.capture.set_portals(&[f.portal]).unwrap();
    wait_strips(&ipc, 1);
    f.enter();
    f.start(CaptureId(1));
    set_scale(&ipc, &first, 2.0);
    if lost_within(&f, CaptureId(1), Duration::from_millis(300)) {
        eprintln!("the compositor ended the capture itself when the scale changed");
        return;
    }
    // The capture lives until the engine's next set, which no longer fits its strip.
    assert_capture_flows(&mut f);
    f.capture.set_portals(&[f.portal]).unwrap();
    f.ended(CaptureId(1), EndReason::Lost);
    let replaced = wait_strips(&ipc, 1);
    assert_eq!((replaced[0].y, replaced[0].h), (4, 36), "{replaced:?}");
}

/// Wait up to `window` for the end of capture `id`; any other end reason than `Lost` is a bug.
fn lost_within(f: &Fixture, id: CaptureId, window: Duration) -> bool {
    let deadline = Instant::now() + window;
    loop {
        match f
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(CaptureEvent::Ended { id: ended, reason }) if ended == id => {
                assert_eq!(reason, EndReason::Lost, "capture {id:?} ended oddly");
                return true;
            }
            Ok(_) => (),
            Err(_) => return false,
        }
    }
}

/// Removing an output while a capture is on another one.
///
/// **On Hyprland 0.56.2, removing any output ends an active capture: the compositor relocates the
/// cursor and drops the pointer lock. The backend reports `Lost` and never reacquires the lock to
/// hide it. The backend itself must not end a capture for an unrelated output removal** (spec
/// §2.8, §3.6.3 as amended).
///
/// The two causes are told apart, not merged: the backend records why it ended each capture
/// (`end_causes_for_test`), and this test asserts that the backend's own decision
/// (`OutputRemoved`, `StripReplaced`) never appears for B's removal, whatever the compositor
/// does. The compositor-induced loss is reported separately (its cause, `Unlocked`, and the
/// cursor's new place); if a Hyprland ever keeps the lock, the capture must simply go on.
///
/// This test alone cannot prove the backend innocent: the compositor's `Unlocked` arrives before
/// the output's `GlobalRemove`, so a backend that wrongly ended the capture itself would find it
/// already ended. The deterministic coverage of the backend's decision is the unit test
/// `removing_another_output_never_ends_the_capture` and
/// `backend_ends_a_capture_only_for_its_own_output`, which simulates the removal.
#[test]
fn output_removal_during_capture() {
    if !dedicated("output_removal_during_capture") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    // The strips go up once the outputs have settled: Hyprland does not move a layer surface when
    // its output is resized later, so one made earlier could sit where the pointer can't reach.
    let Some(second) = SecondOutput::new(&ipc, "wp243d-removal-a") else {
        return;
    };
    settle_outputs(&ipc);
    let mut f = Fixture::new();
    f.portal = CapturePortal {
        from: 4.0,
        to: 60.0,
        ..f.portal
    };
    f.entry_y = Some(30);
    let on_second = CapturePortal {
        id: PortalId(2),
        display: second.display,
        edge: Edge::Right,
        from: 4.0,
        to: 60.0,
    };
    install(&mut f.capture, &[f.portal, on_second]);
    wait_strips(&ipc, 2);
    f.enter();
    f.start(CaptureId(1));
    // The other output goes: its strip goes with it.
    second.remove();
    let first = monitors(&ipc)[0].clone();
    let lost = lost_within(&f, CaptureId(1), Duration::from_millis(500));
    let (rx, ry) = {
        let cursor = ipc.json("cursorpos").unwrap();
        (cursor["x"].as_f64().unwrap(), cursor["y"].as_f64().unwrap())
    };
    // The backend's decision: it never ended this capture for output B's removal.
    let causes = end_causes(&f.capture);
    assert!(
        !causes
            .iter()
            .any(|c| matches!(c.as_str(), "OutputRemoved" | "StripReplaced")),
        "the backend ended a capture for an unrelated output's removal: {causes:?}"
    );
    // The compositor's doing, reported on its own.
    eprintln!(
        "output removal during a capture on another output: compositor-induced loss: {}; \
         causes recorded {causes:?}; the cursor is at ({rx}, {ry}) (the output is {}x{})",
        if lost {
            "capture ended Lost"
        } else {
            "none, the capture survived"
        },
        first.width,
        first.height
    );
    assert_eq!(
        lost,
        !causes.is_empty(),
        "an Ended without a recorded cause, or a cause without an Ended: {causes:?}"
    );
    if !lost {
        assert_capture_flows(&mut f);
        f.capture.end(None).unwrap();
        f.ended(CaptureId(1), EndReason::Requested);
    }
    let left = wait_strips(&ipc, 1);
    assert_eq!(left[0].monitor, first.name);
    // A set that still names the vanished output is refused whole; nothing changed.
    let e = f.capture.set_portals(&[f.portal, on_second]).unwrap_err();
    assert_eq!(set_portals_failure(&e), SetPortalsFailure::Rejected, "{e}");
    assert_eq!(wait_strips(&ipc, 1), left);
    // The backend captures again. The parent compositor re-tiles the nest after the removal, and
    // Hyprland does not move a layer surface when its output is resized: wait for the outputs to
    // settle and make the strip anew, so it sits at the output's edge as it is now.
    settle_outputs(&ipc);
    f.capture.set_portals(&[]).unwrap();
    f.capture.set_portals(&[f.portal]).unwrap();
    wait_strips(&ipc, 1);
    f.enter();
    f.start(CaptureId(2));
    f.capture.end(None).unwrap();
    f.ended(CaptureId(2), EndReason::Requested);

    // Removing the output a capture is on ends it `Lost`.
    let Some(third) = SecondOutput::new(&ipc, "wp243d-removal-b") else {
        return;
    };
    settle_outputs(&ipc);
    let on_third = CapturePortal {
        id: PortalId(3),
        display: third.display,
        edge: Edge::Right,
        from: 4.0,
        to: 60.0,
    };
    install(&mut f.capture, &[f.portal, on_third]);
    let info = monitor_named(&ipc, &third.name);
    while f.events.try_recv().is_ok() {}
    warp(&mut f.capture, third.display, info.width - 12.0, 30.0);
    f.driver.motion(40.0, 0.0);
    f.wait(|e| {
        matches!(
            e,
            CaptureEvent::EdgePressed {
                portal: PortalId(3),
                ..
            }
        )
    });
    f.capture.begin(CaptureId(3), PortalId(3)).unwrap();
    f.wait(|e| matches!(e, CaptureEvent::Started { .. }));
    third.remove();
    f.ended(CaptureId(3), EndReason::Lost);
    wait_strips(&ipc, 1);
}

/// The backend's own decision on an output's removal, with the compositor's reaction taken out of
/// the picture: the removal is *simulated* (`output_removal_for_test` runs the backend's
/// `GlobalRemove` handling without removing anything), so Hyprland neither relocates the cursor
/// nor drops the pointer lock. Deterministic: B's removal must never finish the capture on A and
/// destroys only B's strips; A's own removal ends the capture, by the backend's decision
/// (`OutputRemoved`), and a loss that the compositor causes is a different cause
/// (`output_removal_during_capture`).
#[test]
fn backend_ends_a_capture_only_for_its_own_output() {
    if !dedicated("backend_ends_a_capture_only_for_its_own_output") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    let Some(b) = SecondOutput::new(&ipc, "wp243d-decision-b") else {
        return;
    };
    settle_outputs(&ipc);
    let mut f = Fixture::new();
    f.portal = CapturePortal {
        from: 4.0,
        to: 60.0,
        ..f.portal
    };
    f.entry_y = Some(30);
    let on_b = CapturePortal {
        id: PortalId(2),
        display: b.display,
        ..f.portal
    };
    let first = monitors(&ipc)[0].clone();
    install(&mut f.capture, &[f.portal, on_b]);
    wait_strips(&ipc, 2);
    f.enter();
    f.start(CaptureId(1));
    // B's removal, as the backend handles it: B's strip goes, the capture on A does not.
    require_nest();
    f.capture.output_removal_for_test(&b.name).unwrap();
    assert!(
        !lost_within(&f, CaptureId(1), Duration::from_millis(400)),
        "the backend ended a capture on A for B's removal"
    );
    assert_capture_flows(&mut f);
    assert_eq!(end_causes(&f.capture), Vec::<String>::new());
    let left = wait_strips(&ipc, 1);
    assert_eq!(
        left[0].monitor, first.name,
        "B's strip should be the one that went"
    );
    // A's own removal ends it, and that is the backend's decision.
    f.capture.output_removal_for_test(&first.name).unwrap();
    f.ended(CaptureId(1), EndReason::Lost);
    assert_eq!(end_causes(&f.capture), vec!["OutputRemoved".to_owned()]);
    wait_strips(&ipc, 0);
}

#[test]
fn warp_without_capture_reports_result() {
    if !dedicated("warp_without_capture_reports_result") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    let mut capture = bare_backend();
    let first = monitors(&ipc)[0].clone();
    let p = PointDevice::new(100.0, 50.0);
    // No capture is active: a plain warp, `Ok`, and the cursor is there.
    capture
        .end(Some((DisplayId(first.id), p)))
        .expect("a plain warp reports its result");
    assert_cursor(&ipc, &first, p);
    // An unknown display and a non-finite point are errors, not silent skips.
    assert!(matches!(
        capture.end(Some((DisplayId(9999), p))),
        Err(PlatformError::NotFound)
    ));
    assert!(
        capture
            .end(Some((DisplayId(first.id), PointDevice::new(f64::NAN, 1.0))))
            .is_err()
    );
    // The cursor is translated by the output's position: check it on an output that is not at
    // the origin.
    let Some(second) = SecondOutput::new(&ipc, "wp243d-warp") else {
        eprintln!("the translation by an output position is not exercised: no second output");
        return;
    };
    settle_outputs(&ipc);
    let info = monitor_named(&ipc, &second.name);
    assert!(info.x != 0.0 || info.y != 0.0, "{info:?} is at the origin");
    for point in [
        PointDevice::new(30.0, 20.0),
        PointDevice::new(info.width / 2.0, info.height / 2.0),
        PointDevice::new(info.width - 1.0, info.height - 1.0),
    ] {
        warp(&mut capture, second.display, point.x, point.y);
        assert_cursor(&ipc, &info, point);
    }
    let first = monitor_named(&ipc, &first.name);
    warp(&mut capture, DisplayId(first.id), 20.0, 10.0);
    assert_cursor(&ipc, &first, PointDevice::new(20.0, 10.0));
}

#[test]
fn cursor_position_agrees_with_cursorpos_after_warp_onto_second_output() {
    if !dedicated("cursor_position_agrees_with_cursorpos_after_warp_onto_second_output") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    let mut capture = bare_backend();
    let Some(second) = SecondOutput::new(&ipc, "wp243d-readback") else {
        return;
    };
    settle_outputs(&ipc);
    let info = monitor_named(&ipc, &second.name);
    // The second output is placed beside the first: its origin is not (0, 0), so the read-back
    // has to translate by it.
    assert!(info.x != 0.0 || info.y != 0.0, "{info:?}");
    for (x, y) in [
        (5.0, 5.0),
        (200.0, 80.0),
        (info.width - 2.0, info.height - 2.0),
    ] {
        warp(&mut capture, second.display, x, y);
        assert_cursor(&ipc, &info, PointDevice::new(x, y));
    }
}

#[test]
fn edge_press_after_warp_to_strip_begins() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    let (w, h) = (
        f64::from(f.driver.state.width),
        f64::from(f.driver.state.height),
    );
    // Start away from the edge, then warp to just short of the strip and push into it.
    f.driver.absolute(f.portal.edge, false);
    while f.events.try_recv().is_ok() {}
    f.capture
        .end(Some((
            f.portal.display,
            PointDevice::new(w - 12.0, h / 2.0),
        )))
        .unwrap();
    assert!(
        f.events
            .recv_timeout(Duration::from_millis(150))
            .is_err_and(|e| e == mpsc::RecvTimeoutError::Timeout),
        "the warp alone pressed the strip"
    );
    f.driver.motion(40.0, 0.0);
    f.wait(|e| matches!(e, CaptureEvent::EdgePressed { portal, .. } if *portal == f.portal.id));
    f.capture.begin(CaptureId(1), f.portal.id).unwrap();
    f.wait(|e| matches!(e, CaptureEvent::Started { id } if *id == CaptureId(1)));
    f.capture.end(None).unwrap();
    f.ended(CaptureId(1), EndReason::Requested);
}

#[test]
fn capture_cursor_drift() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    let mut f = Fixture::new();
    let ipc = ipc();
    f.enter();
    let (_, before) = cursor_position(&ipc).unwrap();
    f.start(CaptureId(1));
    for _ in 0..40 {
        f.driver.motion(-25.0, 10.0);
    }
    let (_, during) = cursor_position(&ipc).unwrap();
    f.capture.end(None).unwrap();
    f.ended(CaptureId(1), EndReason::Requested);
    let (_, after) = cursor_position(&ipc).unwrap();
    // F5 (reported, not asserted): the cursor keeps moving, invisibly, during a capture, so an
    // `end` without a warp leaves it wherever it drifted.
    eprintln!(
        "F5: cursor at the strip {before:?}; after 40 relative motions of (-25, 10) during the \
         capture {during:?}; after end(None) {after:?}; drifted: {}",
        before != after
    );
}

#[test]
fn cursor_relocates_when_output_destroyed() {
    if !dedicated("cursor_relocates_when_output_destroyed") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    let mut capture = bare_backend();
    let Some(second) = SecondOutput::new(&ipc, "wp243d-relocate") else {
        return;
    };
    settle_outputs(&ipc);
    warp(&mut capture, second.display, 40.0, 40.0);
    assert_eq!(cursor_position(&ipc).unwrap().0, second.display);
    second.remove();
    // U3: Hyprland moves a cursor off a destroyed output onto a remaining one.
    let deadline = Instant::now() + Duration::from_secs(2);
    let (display, at) = loop {
        match cursor_position(&ipc) {
            Ok((display, at)) if display != second.display => break (display, at),
            other => {
                assert!(
                    Instant::now() < deadline,
                    "U3 does not hold: the cursor stayed off every output: {other:?}, cursorpos {:?}",
                    ipc.json("cursorpos").unwrap()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    eprintln!("U3: the cursor was relocated to {display:?} at {at:?}");
}

// A toplevel of the test's own, to watch which input reaches an ordinary window.
#[derive(Default)]
struct ToplevelState {
    sync: u64,
    acked: bool,
    configured: Option<(u32, u32)>,
    keyboard_focus: bool,
    pointer_focus: bool,
    keys: u32,
    motions: u32,
    buttons: u32,
    move_on_primary: bool,
    seat: Option<wl_seat::WlSeat>,
    toplevel: Option<XdgToplevel>,
}
struct Toplevel {
    conn: Connection,
    queue: EventQueue<ToplevelState>,
    state: ToplevelState,
    qh: QueueHandle<ToplevelState>,
    token: u64,
    _surface: wl_surface::WlSurface,
    _xdg: XdgSurface,
    _toplevel: XdgToplevel,
    _buffer: wl_buffer::WlBuffer,
}
impl Toplevel {
    /// Map a window and wait until it holds keyboard focus.
    fn new() -> Self {
        let conn = Connection::connect_to_env().unwrap();
        let (globals, mut queue) = registry_queue_init::<ToplevelState>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 7..=9, ()).unwrap();
        let wm: XdgWmBase = globals.bind(&qh, 1..=6, ()).unwrap();
        let _keyboard = seat.get_keyboard(&qh, ());
        let _pointer = seat.get_pointer(&qh, ());
        let surface = compositor.create_surface(&qh, ());
        let xdg = wm.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_title("crosspane-capture-test-toplevel".into());
        toplevel.set_app_id("crosspane-capture-test-toplevel".into());
        surface.commit();
        let mut state = ToplevelState {
            seat: Some(seat),
            toplevel: Some(toplevel.clone()),
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !state.acked {
            driver_pump(&conn, &mut queue, &mut state);
            assert!(Instant::now() < deadline, "toplevel configure timed out");
        }
        let (width, height) = state
            .configured
            .filter(|(w, h)| *w > 0 && *h > 0)
            .unwrap_or((320, 200));
        let bytes = width as usize * height as usize * 4;
        let mut file = File::from(
            rustix::fs::memfd_create("capture-test-toplevel", rustix::fs::MemfdFlags::CLOEXEC)
                .unwrap(),
        );
        // Opaque grey.
        file.write_all(&vec![0x40; bytes]).unwrap();
        let pool = shm.create_pool(file.as_fd(), bytes as i32, &qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Xrgb8888,
            &qh,
            (),
        );
        pool.destroy();
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        let mut t = Self {
            conn,
            queue,
            state,
            qh,
            token: 0,
            _surface: surface,
            _xdg: xdg,
            _toplevel: toplevel,
            _buffer: buffer,
        };
        t.sync();
        assert!(
            t.wait(|s| s.keyboard_focus),
            "the toplevel never got keyboard focus"
        );
        t
    }
    fn pump(&mut self) {
        driver_pump(&self.conn, &mut self.queue, &mut self.state);
    }
    fn sync(&mut self) {
        self.token += 1;
        self.conn.display().sync(&self.qh, self.token);
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.state.sync < self.token {
            self.pump();
            assert!(Instant::now() < deadline, "toplevel sync timed out");
        }
    }
    /// Pump events for up to two seconds until `done`.
    fn wait(&mut self, done: impl Fn(&ToplevelState) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done(&self.state) {
            if Instant::now() >= deadline {
                return false;
            }
            self.pump();
        }
        true
    }
    /// What the window has received so far, after flushing everything the compositor sent.
    fn received(&mut self) -> (u32, u32, u32) {
        self.sync();
        (self.state.keys, self.state.motions, self.state.buttons)
    }
}

/// Opt-in prerequisite for R3. The parent has no compositor endpoints; it creates one fresh
/// script-owned nest and gives only its verified endpoints to the measurement child.
fn owned_drag_test(test: &str) -> bool {
    if std::env::var("CROSSPANE_WP255_DROP_PROBE").as_deref() != Ok("1") {
        return false;
    }
    if std::env::var("CROSSPANE_CAPTURE_CHILD").as_deref() != Ok(test) {
        let _exclusive = lock_file(
            "crosspane-capture-topology.lock",
            rustix::fs::FlockOperation::LockExclusive,
        );
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wrapper = root.join("scripts/lead/impl-env.sh");
        let script = root.join("scripts/hypr-nested.sh");
        let name = format!("wp255-drop-probe-{}", std::process::id());
        let state = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
            .join(format!("crosspane-hypr-{name}"));
        assert!(!state.exists(), "refusing a pre-existing nest state");
        struct Stop {
            wrapper: PathBuf,
            script: PathBuf,
            name: String,
        }
        impl Drop for Stop {
            fn drop(&mut self) {
                // The script rechecks its recorded PID/start before any signal.
                let _ = Command::new(&self.wrapper)
                    .arg(&self.script)
                    .args(["stop", "--name", &self.name])
                    .status();
            }
        }
        let _stop = Stop {
            wrapper: wrapper.clone(),
            script: script.clone(),
            name: name.clone(),
        };
        assert!(
            Command::new(&wrapper)
                .arg(&script)
                .args(["start", "--name", &name])
                .status()
                .unwrap()
                .success()
        );
        let exports = Command::new(&wrapper)
            .arg(&script)
            .args(["env", "--name", &name])
            .output()
            .unwrap();
        assert!(exports.status.success());
        let mut child = Command::new(&wrapper);
        child.arg("env");
        for line in String::from_utf8(exports.stdout).unwrap().lines() {
            if line == "unset WAYLAND_SOCKET" {
                continue;
            }
            let (key, value) = line
                .strip_prefix("export ")
                .unwrap()
                .split_once('=')
                .unwrap();
            assert!(matches!(
                key,
                "WAYLAND_DISPLAY" | "HYPRLAND_INSTANCE_SIGNATURE" | "CROSSPANE_NESTED_HYPR"
            ));
            assert!(
                value
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
            );
            child.arg(format!("{key}={value}"));
        }
        let result = child
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("CROSSPANE_CAPTURE_CHILD", test)
            .output()
            .unwrap();
        eprint!("{}", String::from_utf8_lossy(&result.stderr));
        assert!(
            result.status.success(),
            "{test} failed:\n{}",
            String::from_utf8_lossy(&result.stdout)
        );
        return false;
    }
    assert!(nested()); // PID/start, lock PID/display and both sockets, before any connection.
    settle_outputs(&ipc());
    true
}

#[test]
fn native_atomic_nudge_preserves_fractional_position_and_intervening_motion() {
    if !owned_drag_test("native_atomic_nudge_preserves_fractional_position_and_intervening_motion")
    {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    let mut driver = Driver::new();
    fn unrounded(ipc: &HyprIpc) -> [f64; 2] {
        // eval normally returns only "ok". A deliberate test-only error exposes the public
        // query's two unrounded numbers without adding compositor state or a new binding.
        let reply = ipc.request("eval local p = hl.get_cursor_pos() error(string.format('WP255_POSITION %.17g %.17g', p.x, p.y))").unwrap();
        let values: Vec<_> = reply
            .split_once("WP255_POSITION ")
            .unwrap()
            .1
            .split_whitespace()
            .take(2)
            .map(|v| v.parse::<f64>().unwrap())
            .collect();
        [values[0], values[1]]
    }
    const NUDGE: &str = "local p = hl.get_cursor_pos() if p then hl.dispatch(hl.dsp.cursor.move({x = p.x, y = p.y})) end";
    for (x, y) in [(100.25, 100.75), (200.125, 300.875), (500.5, 400.25)] {
        ipc.eval(&format!("hl.dispatch(hl.dsp.cursor.move({{x={x},y={y}}}))"))
            .unwrap();
        let before = unrounded(&ipc);
        assert_eq!(before, [x, y]);
        let rounded = ipc.json("cursorpos").unwrap();
        assert_ne!(rounded["x"].as_f64().unwrap(), before[0]);
        ipc.eval(NUDGE).unwrap();
        let after = unrounded(&ipc);
        assert_eq!(after, before);
        // A sample read before this motion is now stale; the atomic nudge must use the new
        // compositor position, preserving both the fractional remainder and actual travel.
        driver.motion(2.5, 1.25);
        let moved = unrounded(&ipc);
        assert_ne!(moved, before);
        ipc.eval(NUDGE).unwrap();
        let nudged = unrounded(&ipc);
        assert_eq!(nudged, moved);
        eprintln!(
            "atomic nudge: before {before:?}, rounded {rounded}, after {after:?}; intervening motion {moved:?}, after nudge {nudged:?}"
        );
    }
}

#[test]
fn native_drag_drop_nudge_waits_for_release_without_stopping_native_move() {
    if !owned_drag_test("native_drag_drop_nudge_waits_for_release_without_stopping_native_move") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    ipc.eval("hl.bind(\"SUPER + mouse:272\", hl.dsp.window.drag(), { mouse = true })")
        .unwrap();
    fn edge(f: &Fixture, budget: Duration) -> Option<CaptureEvent> {
        let deadline = Instant::now() + budget;
        loop {
            match f
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(event @ CaptureEvent::EdgePressed { .. }) => return Some(event),
                Ok(_) => (),
                Err(_) => return None,
            }
        }
    }
    for path in ["bindm", "xdg"] {
        let driver = Driver::bare();
        let mut window = Toplevel::new();
        let mut f = Fixture::with_driver(driver, Arc::new(|_| {}));
        let client = ipc.json("activewindow").unwrap();
        let address = client["address"].as_str().unwrap();
        assert!(
            address
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() || ch == 'x')
        );
        ipc.eval(&format!(
            "hl.dispatch(hl.dsp.window.float({{window=\"address:{address}\",action=\"enable\"}}))"
        ))
        .unwrap();
        ipc.eval(&format!("hl.dispatch(hl.dsp.window.move({{window=\"address:{address}\",x=200,y=100,relative=false}}))")).unwrap();
        f.driver.absolute_at(280, 180);
        let normal_before = window.received().2;
        f.driver.button(true);
        f.driver.button(false);
        assert!(
            window.wait(|state| state.buttons >= normal_before + 2),
            "{path}: ordinary pre-drag fixture click missing"
        );
        let before = ipc.json("activewindow").unwrap();
        let cursor_before = ipc.json("cursorpos").unwrap();
        if path == "bindm" {
            f.driver.keyboard.key(f.driver.time(), 125, 1);
            f.driver.keyboard.modifiers(64, 0, 0, 0);
            f.driver.sync();
        } else {
            window.state.move_on_primary = true;
        }
        f.driver.button(true);
        if path == "xdg" {
            assert!(window.wait(|state| !state.move_on_primary));
            window.sync();
        }
        f.driver.absolute_at(320, 200);
        let moving = ipc.json("activewindow").unwrap();
        let cursor_moving = ipc.json("cursorpos").unwrap();
        assert_eq!(moving["address"], before["address"]);
        assert_eq!(moving["size"], before["size"]);
        for (axis, key) in ["x", "y"].into_iter().enumerate() {
            let delta = cursor_moving[key].as_f64().unwrap() - cursor_before[key].as_f64().unwrap();
            let window_delta =
                moving["at"][axis].as_f64().unwrap() - before["at"][axis].as_f64().unwrap();
            assert!(
                delta.abs() >= 4.0 && (delta - window_delta).abs() <= 1.0,
                "{path}: no coherent native drag"
            );
        }
        eprintln!(
            "R2 refusal probe {path}: window {} -> {}, cursor {} -> {}, size {}",
            before["at"], moving["at"], cursor_before, cursor_moving, moving["size"]
        );
        f.driver.absolute(Edge::Right, true);
        assert!(
            edge(&f, Duration::from_millis(50)).is_none(),
            "{path}: strip entered before nudges"
        );
        let cursor = ipc.json("cursorpos").unwrap();
        let nudge = "local p = hl.get_cursor_pos() if p then hl.dispatch(hl.dsp.cursor.move({x = p.x, y = p.y})) end";
        for count in 1..=10 {
            let started = Instant::now();
            ipc.eval(nudge).unwrap();
            let event = edge(
                &f,
                Duration::from_millis(50).saturating_sub(started.elapsed()),
            );
            eprintln!(
                "R2 refusal {path} held nudge {count}/10: /eval {nudge}; event {event:?}; elapsed {:?}",
                started.elapsed()
            );
            assert!(
                event.is_none(),
                "{path}: held nudge incorrectly entered strip"
            );
            assert_eq!(ipc.json("cursorpos").unwrap(), cursor);
        }
        let released = Instant::now();
        f.driver.button(false); // Native drag ends by its own release; no drag() IPC anywhere here.
        let cursor = ipc.json("cursorpos").unwrap();
        let nudged = Instant::now();
        ipc.eval(nudge).unwrap();
        let event = edge(&f, Duration::from_millis(150));
        eprintln!(
            "R2 refusal {path} released nudges1 event {event:?}, release elapsed {:?}, nudge elapsed {:?}, drop at {} size {} floating {}",
            released.elapsed(),
            nudged.elapsed(),
            ipc.json("activewindow").unwrap()["at"],
            ipc.json("activewindow").unwrap()["size"],
            ipc.json("activewindow").unwrap()["floating"]
        );
        assert!(
            matches!(event, Some(CaptureEvent::EdgePressed { portal, .. }) if portal == f.portal.id),
            "{path}: release did not enter within one nudge"
        );
        assert_eq!(ipc.json("cursorpos").unwrap(), cursor);
        if path == "bindm" {
            f.driver.keyboard.key(f.driver.time(), 125, 0);
            f.driver.keyboard.modifiers(0, 0, 0, 0);
            f.driver.sync();
        }
    }
}

#[test]
fn native_refused_drags_publish_one_drop_for_bindm_xdg_float_and_tile() {
    if !owned_drag_test("native_refused_drags_publish_one_drop_for_bindm_xdg_float_and_tile") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    ipc.eval("hl.bind(\"SUPER + mouse:272\", hl.dsp.window.drag(), { mouse = true })")
        .unwrap();
    for path in ["bindm", "xdg"] {
        for floating in [true, false] {
            let driver = Driver::bare();
            let mut window = Toplevel::new();
            let mut f = Fixture::with_driver(driver, Arc::new(|_| {}));
            let client = ipc.json("activewindow").unwrap();
            let address = client["address"].as_str().unwrap();
            assert!(
                address
                    .chars()
                    .all(|ch| ch.is_ascii_hexdigit() || ch == 'x')
            );
            let action = if floating { "enable" } else { "disable" };
            ipc.eval(&format!(
                "hl.dispatch(hl.dsp.window.float({{window=\"address:{address}\",action=\"{action}\"}}))"
            ))
            .unwrap();
            if floating {
                ipc.eval(&format!("hl.dispatch(hl.dsp.window.move({{window=\"address:{address}\",x=200,y=100,relative=false}}))")).unwrap();
            }
            assert_eq!(ipc.json("activewindow").unwrap()["floating"], floating);
            f.driver.absolute_at(300, 190);
            f.driver.absolute_at(280, 180);
            let clicks = window.received().2;
            f.driver.button(true);
            f.driver.button(false);
            assert!(
                window.wait(|state| state.buttons >= clicks + 2),
                "{path}/{floating}: initial focus/click control failed"
            );
            if path == "bindm" {
                f.driver.keyboard.key(f.driver.time(), 125, 1);
                f.driver.keyboard.modifiers(64, 0, 0, 0);
                f.driver.sync();
            } else {
                window.state.move_on_primary = true;
            }
            f.driver.button(true);
            if path == "xdg" {
                assert!(window.wait(|state| !state.move_on_primary));
                window.sync();
            }
            // The first motion floats a native tile and changes its size: reset that sample.
            f.driver.absolute_at(320, 200);
            std::thread::sleep(Duration::from_millis(500));
            let before = ipc.json("activewindow").unwrap();
            let cursor_before = ipc.json("cursorpos").unwrap();
            f.driver.absolute_at(360, 220);
            let moving = ipc.json("activewindow").unwrap();
            let cursor = ipc.json("cursorpos").unwrap();
            assert_eq!(moving["address"], before["address"]);
            assert_eq!(moving["size"], before["size"]);
            for (axis, key) in ["x", "y"].into_iter().enumerate() {
                let delta = cursor[key].as_f64().unwrap() - cursor_before[key].as_f64().unwrap();
                let moved =
                    moving["at"][axis].as_f64().unwrap() - before["at"][axis].as_f64().unwrap();
                assert!(delta.abs() >= 4.0 && (delta - moved).abs() <= 1.0);
            }
            std::thread::sleep(Duration::from_millis(120));
            f.driver.absolute(Edge::Right, true);
            let detected = f.wait(|e| matches!(e, CaptureEvent::DragAtEdge { .. }));
            let CaptureEvent::DragAtEdge {
                portal,
                position,
                window: id,
                grab,
                ..
            } = detected
            else {
                unreachable!();
            };
            assert_eq!(portal, f.portal.id);
            let started = Instant::now();
            assert!(matches!(
                f.capture
                    .begin_drag(CaptureId(99), portal, MouseButton::PRIMARY),
                Err(PlatformError::PointerButtonHeld)
            ));
            assert!(started.elapsed() < Duration::from_millis(40));
            let refused = started.elapsed();
            // Refusal must leave the native move running, with no lock or fabricated press.
            let before = ipc.json("activewindow").unwrap();
            f.driver.motion(0.0, 4.0);
            let moving = ipc.json("activewindow").unwrap();
            assert!(
                (moving["at"][1].as_f64().unwrap() - before["at"][1].as_f64().unwrap() - 4.0).abs()
                    <= 1.0
            );
            let held = Instant::now();
            while held.elapsed() < Duration::from_millis(550) {
                if let Ok(event) = f.events.recv_timeout(Duration::from_millis(20)) {
                    assert!(
                        !matches!(
                            event,
                            CaptureEvent::DragDroppedAtEdge { .. }
                                | CaptureEvent::Started { .. }
                                | CaptureEvent::Button { .. }
                        ),
                        "{path}/{floating}: held event {event:?}"
                    );
                }
            }
            let released = Instant::now();
            f.driver.button(false);
            let dropped = f.wait(|e| matches!(e, CaptureEvent::DragDroppedAtEdge { .. }));
            assert!(released.elapsed() < Duration::from_millis(150));
            let drop_latency = released.elapsed();
            assert!(
                matches!(dropped, CaptureEvent::DragDroppedAtEdge { portal: p, position: s, window: w, grab: g, .. } if p == portal && s == position && w == id && g == grab)
            );
            let until = Instant::now() + Duration::from_millis(150);
            while Instant::now() < until {
                if let Ok(event) = f
                    .events
                    .recv_timeout(until.saturating_duration_since(Instant::now()))
                {
                    assert!(
                        !matches!(
                            event,
                            CaptureEvent::DragDroppedAtEdge { .. }
                                | CaptureEvent::Started { .. }
                                | CaptureEvent::Button { .. }
                        ),
                        "{path}/{floating}: repeated/active event {event:?}"
                    );
                }
            }
            if path == "bindm" {
                f.driver.keyboard.key(f.driver.time(), 125, 0);
                f.driver.keyboard.modifiers(0, 0, 0, 0);
                f.driver.sync();
            }
            let post = ipc.json("activewindow").unwrap();
            eprintln!(
                "native watch {path}/{floating}: refused {:?}; one drop {:?} after release {:?}; post at {} size {} floating {}",
                refused, dropped, drop_latency, post["at"], post["size"], post["floating"]
            );
            let clicks = window.received().2;
            f.driver.absolute_at(280, 180);
            f.driver.button(true);
            f.driver.button(false);
            window.sync();
            eprintln!(
                "native watch {path}/{floating}: later click buttons {} -> {}, pointer focus {}, actual client {}",
                clicks,
                window.received().2,
                window.state.pointer_focus,
                ipc.json("activewindow").unwrap()["at"]
            );
        }
    }
}

#[test]
fn native_worker_cancellation_and_repeated_refusals_preserve_watch_bounds() {
    if !owned_drag_test("native_worker_cancellation_and_repeated_refusals_preserve_watch_bounds") {
        return;
    }
    let _guard = serialize();
    let ipc = ipc();
    ipc.eval("hl.bind(\"SUPER + mouse:272\", hl.dsp.window.drag(), { mouse = true })")
        .unwrap();
    let driver = Driver::bare();
    let mut window = Toplevel::new();
    let mut f = Fixture::with_driver(driver, Arc::new(|_| {}));
    let client = ipc.json("activewindow").unwrap();
    let address = client["address"].as_str().unwrap();
    assert!(
        address
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() || ch == 'x')
    );
    ipc.eval(&format!(
        "hl.dispatch(hl.dsp.window.float({{window=\"address:{address}\",action=\"enable\"}}))"
    ))
    .unwrap();
    ipc.eval(&format!("hl.dispatch(hl.dsp.window.move({{window=\"address:{address}\",x=200,y=100,relative=false}}))")).unwrap();
    f.driver.absolute_at(280, 180);
    f.driver.button(true);
    f.driver.button(false);
    assert!(window.wait(|state| state.buttons >= 2));
    f.driver.keyboard.key(f.driver.time(), 125, 1);
    f.driver.keyboard.modifiers(64, 0, 0, 0);
    f.driver.sync();
    f.driver.button(true);
    std::thread::sleep(Duration::from_millis(120));
    f.driver.absolute_at(320, 200);
    std::thread::sleep(Duration::from_millis(120));
    f.driver.absolute(Edge::Right, true);
    f.wait(|e| matches!(e, CaptureEvent::DragAtEdge { .. }));
    // This owned nest's public Lua function counts actual production nudge requests. No new
    // backend hook or fake worker: refusals travel through HyprlandCapture's real command queue.
    ipc.eval("WP255_NUDGES = 0 WP255_QUERY = hl.get_cursor_pos hl.get_cursor_pos = function() WP255_NUDGES = WP255_NUDGES + 1 return WP255_QUERY() end").unwrap();
    fn count(ipc: &HyprIpc) -> u64 {
        let reply = ipc
            .request("eval error('WP255_COUNT ' .. tostring(WP255_NUDGES))")
            .unwrap();
        reply
            .rsplit_once("WP255_COUNT ")
            .unwrap()
            .1
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }
    let refuse = |f: &mut Fixture| {
        assert!(matches!(
            f.capture
                .begin_drag(CaptureId(99), f.portal.id, MouseButton::PRIMARY),
            Err(PlatformError::PointerButtonHeld)
        ));
    };
    refuse(&mut f);
    f.driver
        .absolute_at(f.driver.state.width - 12, f.driver.state.height / 2);
    std::thread::sleep(Duration::from_millis(65));
    let cancelled = count(&ipc);
    for _ in 0..3 {
        refuse(&mut f);
        std::thread::sleep(Duration::from_millis(30));
    }
    f.capture.set_portals(&[f.portal]).unwrap(); // FIFO barrier through the real worker.
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(
        count(&ipc),
        cancelled,
        "delayed refusals rearmed a cancelled hit"
    );
    assert!(
        !f.events
            .try_iter()
            .any(|e| matches!(e, CaptureEvent::DragDroppedAtEdge { .. }))
    );
    f.driver.absolute(Edge::Right, true);
    f.wait(|e| matches!(e, CaptureEvent::DragAtEdge { .. }));
    ipc.eval("WP255_NUDGES = 0").unwrap();
    let armed = Instant::now();
    refuse(&mut f);
    for _ in 0..10 {
        refuse(&mut f);
        std::thread::sleep(Duration::from_millis(30));
    }
    f.capture.set_portals(&[f.portal]).unwrap();
    let elapsed = armed.elapsed();
    let nudges = count(&ipc);
    assert!(
        nudges > 0 && nudges <= 1 + elapsed.as_millis() as u64 / 50,
        "{nudges} nudges in {elapsed:?}"
    );
    std::thread::sleep(Duration::from_millis(10060).saturating_sub(armed.elapsed()));
    let expired = count(&ipc);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        count(&ipc),
        expired,
        "duplicate refusals extended the ten-second watch"
    );
    f.driver.button(false);
    ipc.eval("local p = hl.get_cursor_pos() if p then hl.dispatch(hl.dsp.cursor.move({x = p.x, y = p.y})) end").unwrap();
    f.wait(|e| matches!(e, CaptureEvent::EdgePressed { .. }));
    std::thread::sleep(Duration::from_millis(50));
    assert!(!f.events.try_iter().any(|e| matches!(
        e,
        CaptureEvent::DragDroppedAtEdge { .. } | CaptureEvent::Started { .. }
    )));
    f.driver.keyboard.key(f.driver.time(), 125, 0);
    f.driver.keyboard.modifiers(0, 0, 0, 0);
    f.driver.sync();
    eprintln!(
        "real worker: cancelled nudge count {cancelled} unchanged after 3 delayed refusals; 10 duplicate refusals produced {nudges} nudges in {elapsed:?}; watch stopped at {expired} nudges by {:?}, no drop after expiry/release",
        armed.elapsed()
    );
}

#[test]
fn native_drag_plain_capture_and_rollback_remain_unchanged() {
    if !owned_drag_test("native_drag_plain_capture_and_rollback_remain_unchanged") {
        return;
    }
    hundred_crossings();
    closed_gate_and_button_guard();
    abort_from_another_thread();
    portal_replacement_is_atomic();
}

#[test]
fn exclusive_strip_swallows_virtual_input() {
    if !nested() {
        return;
    }
    let _guard = serialize();
    // No observer surface covers the output: the test's window is what the input is for.
    let driver = Driver::bare();
    let mut window = Toplevel::new();
    let mut f = Fixture::with_driver(driver, Arc::new(|_| {}));
    let (w, h) = (f.driver.state.width, f.driver.state.height);
    let centre = |f: &mut Fixture| {
        f.driver
            .pointer
            .motion_absolute(f.driver.time(), w / 2, h / 2, w, h);
        f.driver.pointer.frame();
        f.driver.sync();
    };
    let key = |f: &mut Fixture| {
        // evdev KEY_A, pressed and released.
        f.driver.keyboard.key(f.driver.time(), 30, 1);
        f.driver.keyboard.key(f.driver.time(), 30, 0);
        f.driver.sync();
    };
    let click = |f: &mut Fixture| {
        f.driver.button(true);
        f.driver.button(false);
    };
    // Control: with nothing captured the same virtual devices do reach the window.
    centre(&mut f);
    f.driver.motion(1.0, 1.0);
    key(&mut f);
    click(&mut f);
    let open = window.received();
    assert!(
        open.0 >= 2 && open.1 >= 1 && open.2 >= 2,
        "virtual input did not reach an ordinary window: {open:?}"
    );
    // Capture through the strip.
    f.enter();
    f.start(CaptureId(1));
    let swallowed = window.received();
    key(&mut f);
    f.driver.motion(3.0, 4.0);
    centre(&mut f);
    f.driver.motion(2.0, 1.0);
    click(&mut f);
    // F1-F3: the exclusive strip holds both foci wherever the cursor is.
    assert_eq!(
        window.received(),
        swallowed,
        "the window received virtual input during the capture"
    );
    // The capture, not the window, sees it all. The roundtrips above synchronise the driver and
    // the window with the compositor, not the capture's worker and delivery threads: receive with
    // a deadline until the key, the motion and the button have all arrived, and fail on an `Ended`.
    let mut seen = (false, false, false);
    let deadline = Instant::now() + Duration::from_secs(2);
    while seen != (true, true, true) {
        match f
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(CaptureEvent::Key {
                down: true, usage, ..
            }) if usage == HidUsage::keyboard(0x04) => seen.0 = true,
            Ok(CaptureEvent::Motion { .. }) => seen.1 = true,
            Ok(CaptureEvent::Button { down: true, .. }) => seen.2 = true,
            Ok(CaptureEvent::Ended { id, reason }) => {
                panic!("capture {id:?} ended while input was routed to it: {reason:?}")
            }
            Ok(_) => (),
            Err(e) => panic!(
                "the capture missed input it should own (key, motion, button): {seen:?}: {e}"
            ),
        }
    }
    f.capture.end(None).unwrap();
    f.ended(CaptureId(1), EndReason::Requested);
    // Everything after `end` reaches the window again.
    assert!(
        window.wait(|s| s.keyboard_focus),
        "the window never got the keyboard back"
    );
    let resumed = window.received();
    key(&mut f);
    centre(&mut f);
    f.driver.motion(2.0, 1.0);
    click(&mut f);
    let after = window.received();
    assert!(
        after.0 >= resumed.0 + 2 && after.1 > resumed.1 && after.2 >= resumed.2 + 2,
        "input after end did not reach the window: {resumed:?} -> {after:?}"
    );
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ToplevelState {
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
impl Dispatch<XdgWmBase, ()> for ToplevelState {
    fn event(
        _: &mut Self,
        wm: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}
impl Dispatch<XdgSurface, ()> for ToplevelState {
    fn event(
        s: &mut Self,
        xdg: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            s.acked = true;
        }
    }
}
impl Dispatch<XdgToplevel, ()> for ToplevelState {
    fn event(
        s: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = event {
            s.configured = Some((width as u32, height as u32));
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for ToplevelState {
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
            wl_keyboard::Event::Key { .. } => s.keys += 1,
            _ => (),
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for ToplevelState {
    fn event(
        s: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { .. } => s.pointer_focus = true,
            wl_pointer::Event::Leave { .. } => s.pointer_focus = false,
            wl_pointer::Event::Motion { .. } => s.motions += 1,
            wl_pointer::Event::Button {
                serial,
                button,
                state,
                ..
            } => {
                s.buttons += 1;
                if s.move_on_primary
                    && button == 0x110
                    && state == WEnum::Value(wl_pointer::ButtonState::Pressed)
                {
                    s.move_on_primary = false;
                    s.toplevel
                        .as_ref()
                        .unwrap()
                        ._move(s.seat.as_ref().unwrap(), serial);
                }
            }
            _ => (),
        }
    }
}
impl Dispatch<wl_callback::WlCallback, u64> for ToplevelState {
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
delegate_noop!(ToplevelState: ignore wl_compositor::WlCompositor);
delegate_noop!(ToplevelState: ignore wl_surface::WlSurface);
delegate_noop!(ToplevelState: ignore wl_seat::WlSeat);
delegate_noop!(ToplevelState: ignore wl_shm::WlShm);
delegate_noop!(ToplevelState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(ToplevelState: ignore wl_buffer::WlBuffer);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for DriverState {
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
impl Dispatch<wl_output::WlOutput, ()> for DriverState {
    fn event(
        s: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode {
            flags: WEnum::Value(flags),
            width,
            height,
            ..
        } = event
            && flags.contains(wl_output::Mode::Current)
        {
            s.width = width as u32;
            s.height = height as u32;
        }
    }
}
impl Dispatch<ZwlrLayerSurfaceV1, ()> for DriverState {
    fn event(
        s: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            layer.ack_configure(serial);
            s.size = Some((width, height));
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for DriverState {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { .. } => s.keyboard = true,
            wl_keyboard::Event::Leave { .. } => s.keyboard = false,
            _ => (),
        }
    }
}
impl Dispatch<wl_callback::WlCallback, u64> for DriverState {
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
delegate_noop!(DriverState: ignore wl_compositor::WlCompositor);
delegate_noop!(DriverState: ignore wl_surface::WlSurface);
delegate_noop!(DriverState: ignore wl_seat::WlSeat);
delegate_noop!(DriverState: ignore wl_shm::WlShm);
delegate_noop!(DriverState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(DriverState: ignore wl_buffer::WlBuffer);
impl Dispatch<wl_pointer::WlPointer, ()> for DriverState {
    fn event(
        s: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => s.pointer_pos = Some((surface_x, surface_y)),
            wl_pointer::Event::Leave { .. } => s.pointer_pos = None,
            _ => (),
        }
    }
}
delegate_noop!(DriverState: ignore ZwlrLayerShellV1);
delegate_noop!(DriverState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(DriverState: ignore ZwlrVirtualPointerV1);
delegate_noop!(DriverState: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(DriverState: ignore ZwpVirtualKeyboardV1);
