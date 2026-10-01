//! Self-driven capture conformance in a nested Hyprland only. Never use the owner's pointer.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]
use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, Edge, EndReason, InputCapture, IoGate, MotionKind,
    PlatformError, PortalId,
};
use crosspane_platform_linux::hyprland::{capture::HyprlandCapture, ipc::HyprIpc};
use crosspane_types::{geom::PointDevice, hid::HidUsage, input::ScrollPhase};
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
fn serialize() -> File {
    // Nextest launches separate processes; an in-process mutex cannot protect the shared nest.
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap();
    let path = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
        .join(format!("crosspane-capture-{signature}.lock"));
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
fn nested() -> bool {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: capture needs CROSSPANE_NESTED_HYPR=1 from scripts/hypr-nested.sh env");
        return false;
    }
    true
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
    _surface: wl_surface::WlSurface,
    _layer: ZwlrLayerSurfaceV1,
    _buffer: wl_buffer::WlBuffer,
}
impl Driver {
    fn new() -> Self {
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
        let mut state = DriverState::default();
        let mut queue = queue;
        let deadline = Instant::now() + Duration::from_secs(2);
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
        let mut d = Self {
            conn,
            queue,
            state,
            qh,
            pointer,
            keyboard,
            token: 0,
            _surface: surface,
            _layer: layer,
            _buffer: buffer,
        };
        d.sync();
        d.focus();
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
fn driver_pump(conn: &Connection, queue: &mut EventQueue<DriverState>, state: &mut DriverState) {
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
}
impl Fixture {
    fn new() -> Self {
        Self::with_sink(Arc::new(|_| {}))
    }
    fn with_sink(tap: Arc<dyn Fn(&CaptureEvent) + Send + Sync>) -> Self {
        let mut driver = Driver::new();
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
        let mut capture = HyprlandCapture::new(gate.clone()).unwrap();
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
        }
    }
    fn enter(&mut self) {
        self.driver.absolute(self.portal.edge, false);
        // Drain old idle events before this crossing, never while a capture is active.
        while self.events.try_recv().is_ok() {}
        self.driver.absolute(self.portal.edge, true);
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
                    self.driver.absolute(self.portal.edge, true);
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
    f.capture.set_portals(&[f.portal]).unwrap();
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
