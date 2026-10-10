//! Focused tests against an in-process fake EIS server (`reis::eis`) on a socket pair. The fake
//! plays a compositor: it announces a seat, creates devices when they are bound, resumes them when
//! the client says they are ready, and records what the client sends. No real compositor, portal
//! or session is involved.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crosspane_platform::{IoGate, KeyInjector, PlatformError, PointerInjector};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelSize, PointDevice, PointLogical, SizeMm, VectorLogical,
};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use reis::PendingRequestResult;
use reis::eis;
use reis::enumflags2::BitFlags;
use reis::handshake::EisHandshaker;
use reis::request::{DeviceCapability, EisRequest, EisRequestConverter};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use super::keymap::test_keymap_text;
use super::{DisplaysFn, EisKeyInjector, EisPointerInjector, EisSource};

const WAIT: Duration = Duration::from_secs(3);

// ---- the fake compositor ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Seen {
    Hello { name: String, sender: bool },
    Bound(BitFlags<DeviceCapability>),
    Ready,
    Start(u32),
    Stop,
    Key(u32, bool),
    Motion(f32, f32),
    Button(u32, bool),
    Scroll(f32, f32),
    Discrete(i32, i32),
    ScrollStop { x: bool, y: bool, cancel: bool },
    Frame,
    Disconnect,
}

#[derive(Clone)]
struct FakeDevice {
    name: &'static str,
    caps: BitFlags<DeviceCapability>,
    regions: Vec<(u32, u32, u32, u32)>,
    keymap: Option<String>,
}

enum Ctl {
    Modifiers { device: &'static str, locked: u32 },
    Pause(&'static str),
    Resume(&'static str),
    Hangup,
}

struct Fake {
    seen: Receiver<(String, Seen)>,
    ctl: Option<Sender<Ctl>>,
    thread: Option<JoinHandle<()>>,
}

impl Fake {
    /// Start a compositor on a socket pair; the client end is returned.
    fn start(devices: Vec<FakeDevice>) -> (Fake, OwnedFd) {
        let (client, server) = UnixStream::pair().unwrap();
        let (seen_tx, seen) = mpsc::channel();
        let (ctl, ctl_rx) = mpsc::channel();
        let thread = thread::spawn(move || serve(server, devices, &seen_tx, &ctl_rx));
        (
            Fake {
                seen,
                ctl: Some(ctl),
                thread: Some(thread),
            },
            OwnedFd::from(client),
        )
    }

    /// Attach `source` to a new compositor with `devices` and wait until the handshake, the bind
    /// and every device's `ready` have been seen and the source is live.
    fn connect(source: &EisSource, devices: Vec<FakeDevice>) -> Fake {
        let count = devices.len();
        let (fake, fd) = Fake::start(devices);
        source.attach(fd).unwrap();
        assert_eq!(
            fake.next(),
            (
                String::new(),
                Seen::Hello {
                    name: "Crosspane".into(),
                    sender: true
                }
            )
        );
        assert_eq!(
            fake.next(),
            (
                String::new(),
                Seen::Bound(
                    DeviceCapability::Keyboard
                        | DeviceCapability::Pointer
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Button
                        | DeviceCapability::Scroll
                )
            )
        );
        for _ in 0..count {
            assert_eq!(fake.next().1, Seen::Ready);
        }
        wait_until("the source to go live", || source.is_live());
        fake
    }

    fn next(&self) -> (String, Seen) {
        self.seen
            .recv_timeout(WAIT)
            .expect("the compositor saw nothing")
    }

    /// The next events, in order, as `(device, event)`.
    fn expect(&self, events: &[(&str, Seen)]) {
        for (device, event) in events {
            let (got_device, got) = self.next();
            assert_eq!((got_device.as_str(), &got), (*device, event));
        }
    }

    fn quiet(&self) {
        match self.seen.recv_timeout(Duration::from_millis(150)) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!("expected silence, got {other:?}"),
        }
    }

    fn send(&self, ctl: Ctl) {
        self.ctl.as_ref().unwrap().send(ctl).unwrap();
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.ctl = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn name_of(device: &reis::request::Device) -> String {
    device.name().unwrap_or("?").to_owned()
}

fn serve(
    stream: UnixStream,
    devices: Vec<FakeDevice>,
    seen: &Sender<(String, Seen)>,
    ctl: &Receiver<Ctl>,
) {
    let Ok(ctx) = eis::Context::new(stream) else {
        return;
    };
    let mut shaker = EisHandshaker::new(&ctx, 0);
    let mut converter: Option<EisRequestConverter> = None;
    let mut live: HashMap<&'static str, reis::request::Device> = HashMap::new();
    let say = |device: String, event: Seen| {
        let _ = seen.send((device, event));
    };
    loop {
        let mut fds = [PollFd::new(&ctx, PollFlags::IN)];
        let _ = poll(
            &mut fds,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 5_000_000,
            }),
        );
        if ctx.read().is_err() {
            return;
        }
        while let Some(result) = ctx.pending_request() {
            let PendingRequestResult::Request(request) = result else {
                continue;
            };
            match converter.as_mut() {
                None => {
                    if let Some(response) = shaker.handle_request(request).unwrap() {
                        say(
                            String::new(),
                            Seen::Hello {
                                name: response.name.clone().unwrap_or_default(),
                                sender: response.context_type
                                    == eis::handshake::ContextType::Sender,
                            },
                        );
                        let conv = EisRequestConverter::new(&ctx, response, 0);
                        let _seat = conv.handle().add_seat(Some("seat0"), BitFlags::all());
                        converter = Some(conv);
                    }
                }
                Some(conv) => conv.handle_request(request).unwrap(),
            }
        }
        if let Some(conv) = converter.as_mut() {
            while let Some(request) = conv.next_request() {
                match request {
                    EisRequest::Bind(bind) => {
                        say(String::new(), Seen::Bound(bind.capabilities));
                        for fake in &devices {
                            let device = bind.seat.add_device(
                                Some(fake.name),
                                eis::device::DeviceType::Virtual,
                                fake.caps,
                                |device| {
                                    for &(x, y, w, h) in &fake.regions {
                                        device.device().region(x, y, w, h, 1.0);
                                    }
                                    if let (Some(text), Some(keyboard)) =
                                        (&fake.keymap, device.interface::<eis::Keyboard>())
                                    {
                                        let fd = memfd_with(text.as_bytes());
                                        keyboard.keymap(
                                            eis::keyboard::KeymapType::Xkb,
                                            u32::try_from(text.len() + 1).unwrap(),
                                            fd.as_fd(),
                                        );
                                    }
                                },
                            );
                            live.insert(fake.name, device);
                        }
                    }
                    EisRequest::Ready(ready) => {
                        say(name_of(&ready.device), Seen::Ready);
                        ready.device.resumed();
                    }
                    EisRequest::DeviceStartEmulating(start) => {
                        say(name_of(&start.device), Seen::Start(start.sequence));
                    }
                    EisRequest::DeviceStopEmulating(stop) => {
                        say(name_of(&stop.device), Seen::Stop);
                    }
                    EisRequest::KeyboardKey(key) => say(
                        name_of(&key.device),
                        Seen::Key(key.key, key.state == eis::keyboard::KeyState::Press),
                    ),
                    EisRequest::PointerMotionAbsolute(motion) => say(
                        name_of(&motion.device),
                        Seen::Motion(motion.dx_absolute, motion.dy_absolute),
                    ),
                    EisRequest::Button(button) => say(
                        name_of(&button.device),
                        Seen::Button(
                            button.button,
                            button.state == eis::button::ButtonState::Press,
                        ),
                    ),
                    EisRequest::ScrollDelta(scroll) => {
                        say(name_of(&scroll.device), Seen::Scroll(scroll.dx, scroll.dy));
                    }
                    EisRequest::ScrollDiscrete(scroll) => say(
                        name_of(&scroll.device),
                        Seen::Discrete(scroll.discrete_dx, scroll.discrete_dy),
                    ),
                    EisRequest::ScrollStop(stop) => say(
                        name_of(&stop.device),
                        Seen::ScrollStop {
                            x: stop.x,
                            y: stop.y,
                            cancel: false,
                        },
                    ),
                    EisRequest::ScrollCancel(stop) => say(
                        name_of(&stop.device),
                        Seen::ScrollStop {
                            x: stop.x,
                            y: stop.y,
                            cancel: true,
                        },
                    ),
                    EisRequest::Frame(frame) => say(name_of(&frame.device), Seen::Frame),
                    EisRequest::Disconnect => say(String::new(), Seen::Disconnect),
                    _ => {}
                }
            }
        }
        loop {
            match ctl.try_recv() {
                Ok(Ctl::Modifiers { device, locked }) => {
                    if let (Some(conv), Some(device)) = (converter.as_ref(), live.get(device))
                        && let Some(keyboard) = device.interface::<eis::Keyboard>()
                    {
                        conv.handle()
                            .with_next_serial(|serial| keyboard.modifiers(serial, 0, locked, 0, 0));
                    }
                }
                Ok(Ctl::Pause(device)) => {
                    if let Some(device) = live.get(device) {
                        device.paused();
                    }
                }
                Ok(Ctl::Resume(device)) => {
                    if let Some(device) = live.get(device) {
                        device.resumed();
                    }
                }
                Ok(Ctl::Hangup) => {
                    if let Some(conv) = converter.as_ref() {
                        conv.handle().disconnected(
                            reis::ei::connection::DisconnectReason::Disconnected,
                            None,
                        );
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let _ = ctx.flush();
    }
}

fn memfd_with(bytes: &[u8]) -> OwnedFd {
    let fd = rustix::fs::memfd_create("keymap", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    let mut written = 0;
    while written < bytes.len() {
        written += rustix::io::write(&fd, &bytes[written..]).unwrap();
    }
    // The terminating NUL the size on the wire counts.
    rustix::io::write(&fd, &[0]).unwrap();
    fd
}

// ---- helpers ---------------------------------------------------------------------------------

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn caps(list: &[DeviceCapability]) -> BitFlags<DeviceCapability> {
    list.iter().copied().collect()
}

fn keyboard(keymap: Option<String>) -> FakeDevice {
    FakeDevice {
        name: "keyboard",
        caps: caps(&[DeviceCapability::Keyboard]),
        regions: Vec::new(),
        keymap,
    }
}

fn pointer() -> FakeDevice {
    FakeDevice {
        name: "pointer",
        caps: caps(&[
            DeviceCapability::Pointer,
            DeviceCapability::Button,
            DeviceCapability::Scroll,
        ]),
        regions: Vec::new(),
        keymap: None,
    }
}

fn absolute(name: &'static str, regions: Vec<(u32, u32, u32, u32)>) -> FakeDevice {
    FakeDevice {
        name,
        caps: caps(&[
            DeviceCapability::PointerAbsolute,
            DeviceCapability::Button,
            DeviceCapability::Scroll,
        ]),
        regions,
        keymap: None,
    }
}

fn display(id: u32, pixels: (u32, u32), scale: f64, origin: (f64, f64)) -> DisplayInfo {
    DisplayInfo {
        id: DisplayId(id),
        name: format!("display-{id}"),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(600.0, 340.0),
            pixel_size: PixelSize::new(pixels.0, pixels.1),
            scale,
            logical_origin: PointLogical::new(origin.0, origin.1),
        },
        refresh_millihz: 60_000,
        color_space: ColorSpace::Srgb,
        hdr: false,
    }
}

/// A 1920x1080 display at scale 1 and, to its right, a 3840x1440 one at scale 2 (1920x720 logical).
fn two_displays() -> Vec<DisplayInfo> {
    vec![
        display(1, (1920, 1080), 1.0, (0.0, 0.0)),
        display(2, (3840, 1440), 2.0, (1920.0, 0.0)),
    ]
}

struct Rig {
    gate: Arc<IoGate>,
    source: EisSource,
    keys: EisKeyInjector,
    pointer: EisPointerInjector,
    fake: Fake,
}

fn source_with(
    displays: Vec<DisplayInfo>,
) -> (Arc<IoGate>, EisSource, EisKeyInjector, EisPointerInjector) {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    let snapshot: DisplaysFn = Arc::new(move || displays.clone());
    let (source, keys, pointer) = EisSource::new(gate.clone(), snapshot).unwrap();
    (gate, source, keys, pointer)
}

fn rig_with(devices: Vec<FakeDevice>, displays: Vec<DisplayInfo>) -> Rig {
    let (gate, source, keys, pointer) = source_with(displays);
    let fake = Fake::connect(&source, devices);
    Rig {
        gate,
        source,
        keys,
        pointer,
        fake,
    }
}

/// keyboard, a relative pointer with buttons and scroll, and one absolute device spanning both
/// displays' regions.
fn rig() -> Rig {
    rig_with(
        vec![
            keyboard(None),
            pointer(),
            absolute("absolute", vec![(0, 0, 1920, 1080), (1920, 0, 1920, 720)]),
        ],
        two_displays(),
    )
}

const KEY_A: HidUsage = HidUsage::keyboard(0x04);
const KEY_B: HidUsage = HidUsage::keyboard(0x05);
const SHIFT: HidUsage = HidUsage::keyboard(0xE1);
const CAPS: HidUsage = HidUsage::keyboard(0x39);

fn wheel(v120_x: i32, v120_y: i32) -> ScrollDelta {
    ScrollDelta {
        v120_x,
        v120_y,
        pixels: None,
        phase: ScrollPhase::Discrete,
        stop_x: false,
        stop_y: false,
    }
}

fn smooth(x: f64, y: f64) -> ScrollDelta {
    ScrollDelta {
        v120_x: 0,
        v120_y: 0,
        pixels: Some(VectorLogical::new(x, y)),
        phase: ScrollPhase::Changed,
        stop_x: false,
        stop_y: false,
    }
}

// ---- tests -----------------------------------------------------------------------------------

#[test]
fn attach_handshakes_as_a_sender_binds_and_goes_live() {
    // `rig` asserts the hello (name and sender), the bind mask and a `ready` per device.
    let rig = rig();
    assert!(rig.source.is_live());
    assert_eq!(rig.keys.lock_keys().unwrap(), LockKeys::default());
    rig.fake.quiet();
}

#[test]
fn keys_start_emulating_once_frame_each_request_and_settle_on_release_all() {
    let mut rig = rig();
    rig.keys.key(KEY_A, true).unwrap();
    rig.fake.expect(&[
        ("keyboard", Seen::Start(1)),
        ("keyboard", Seen::Key(30, true)),
        ("keyboard", Seen::Frame),
    ]);
    // A repeated down is not sent again: the compositor supplies the repeat.
    rig.keys.key(KEY_A, true).unwrap();
    rig.fake.quiet();
    rig.keys.key(KEY_A, false).unwrap();
    rig.fake.expect(&[
        ("keyboard", Seen::Key(30, false)),
        ("keyboard", Seen::Frame),
    ]);
    // An up for a key not held is nothing.
    rig.keys.key(KEY_A, false).unwrap();
    rig.fake.quiet();
    // Idle: release_all lets go of the device.
    rig.keys.release_all().unwrap();
    rig.fake.expect(&[("keyboard", Seen::Stop)]);
    rig.keys.release_all().unwrap();
    rig.fake.quiet();

    // A second sequence restarts emulation with a higher number; release_all frees what is held.
    rig.keys.key(SHIFT, true).unwrap();
    rig.keys.key(KEY_B, true).unwrap();
    rig.fake.expect(&[
        ("keyboard", Seen::Start(2)),
        ("keyboard", Seen::Key(42, true)),
        ("keyboard", Seen::Frame),
        ("keyboard", Seen::Key(48, true)),
        ("keyboard", Seen::Frame),
    ]);
    rig.keys.release_all().unwrap();
    rig.fake.expect(&[
        ("keyboard", Seen::Key(42, false)),
        ("keyboard", Seen::Key(48, false)),
        ("keyboard", Seen::Frame),
        ("keyboard", Seen::Stop),
    ]);
}

#[test]
fn a_closed_gate_refuses_every_down_and_lets_ups_through() {
    let mut rig = rig();
    rig.keys.key(KEY_A, true).unwrap();
    rig.fake.expect(&[
        ("keyboard", Seen::Start(1)),
        ("keyboard", Seen::Key(30, true)),
        ("keyboard", Seen::Frame),
    ]);
    rig.gate.set_engine_permits(false);
    for result in [
        rig.keys.key(KEY_B, true),
        rig.keys.set_lock_keys(LockKeys {
            caps_lock: Some(true),
            ..LockKeys::default()
        }),
        rig.pointer.button(MouseButton::PRIMARY, true),
        rig.pointer.scroll(wheel(0, 120)),
        rig.pointer
            .move_to(DisplayId(1), PointDevice::new(5.0, 5.0)),
    ] {
        assert!(matches!(result, Err(PlatformError::Locked)), "{result:?}");
    }
    // The held key is let go, by the worker's gate watch or by this call: exactly once.
    rig.keys.release_all().unwrap();
    rig.fake.expect(&[
        ("keyboard", Seen::Key(30, false)),
        ("keyboard", Seen::Frame),
        ("keyboard", Seen::Stop),
    ]);
    rig.fake.quiet();
    // Open again: input flows.
    rig.gate.set_engine_permits(true);
    rig.keys.key(KEY_B, true).unwrap();
    rig.fake.expect(&[
        ("keyboard", Seen::Start(2)),
        ("keyboard", Seen::Key(48, true)),
        ("keyboard", Seen::Frame),
    ]);
}

#[test]
fn closing_the_gate_releases_held_input_and_ends_smooth_scroll_without_a_call() {
    let mut rig = rig();
    rig.keys.key(KEY_A, true).unwrap();
    rig.pointer.scroll(smooth(0.0, 5.0)).unwrap();
    rig.pointer.button(MouseButton::PRIMARY, true).unwrap();
    for _ in 0..3 + 3 + 2 {
        rig.fake.next();
    }
    rig.gate.set_session_permits(false);
    // Nothing is called: the worker's gate watch lets go of everything.
    let mut seen = Vec::new();
    for _ in 0..8 {
        seen.push(rig.fake.next());
    }
    let events: Vec<&Seen> = seen.iter().map(|(_, e)| e).collect();
    assert!(events.contains(&&Seen::Key(30, false)), "{seen:?}");
    assert!(events.contains(&&Seen::Button(0x110, false)), "{seen:?}");
    assert!(
        events.contains(&&Seen::ScrollStop {
            x: false,
            y: true,
            cancel: false
        }),
        "{seen:?}"
    );
    assert_eq!(
        events.iter().filter(|e| ***e == Seen::Stop).count(),
        2,
        "{seen:?}"
    );
    rig.fake.quiet();
}

#[test]
fn buttons_and_scroll_use_the_last_absolute_device_and_map_signs() {
    let mut rig = rig();
    // Before any motion: the first device with the capability.
    rig.pointer.button(MouseButton::PRIMARY, true).unwrap();
    rig.fake.expect(&[
        ("pointer", Seen::Start(1)),
        ("pointer", Seen::Button(0x110, true)),
        ("pointer", Seen::Frame),
    ]);
    rig.pointer.button(MouseButton::PRIMARY, false).unwrap();
    rig.fake.expect(&[
        ("pointer", Seen::Button(0x110, false)),
        ("pointer", Seen::Frame),
    ]);
    // Motion makes the absolute device the active one.
    rig.pointer
        .move_to(DisplayId(1), PointDevice::new(10.0, 20.0))
        .unwrap();
    rig.fake.expect(&[
        ("absolute", Seen::Start(2)),
        ("absolute", Seen::Motion(10.0, 20.0)),
        ("absolute", Seen::Frame),
    ]);
    for (button, code) in [
        (MouseButton::SECONDARY, 0x111),
        (MouseButton::TERTIARY, 0x112),
        (MouseButton::BACK, 0x113),
        (MouseButton::FORWARD, 0x114),
    ] {
        rig.pointer.button(button, true).unwrap();
        rig.fake.expect(&[
            ("absolute", Seen::Button(code, true)),
            ("absolute", Seen::Frame),
        ]);
        rig.pointer.button(button, false).unwrap();
        rig.fake.expect(&[
            ("absolute", Seen::Button(code, false)),
            ("absolute", Seen::Frame),
        ]);
    }
    assert!(matches!(
        rig.pointer.button(MouseButton(9), true),
        Err(PlatformError::Unsupported(_))
    ));
    // Wheel detents are 120ths with both axes flipped from the HID convention.
    rig.pointer.scroll(wheel(120, -240)).unwrap();
    rig.fake.expect(&[
        ("absolute", Seen::Discrete(-120, 240)),
        ("absolute", Seen::Frame),
    ]);
    // A smooth gesture: pixels, then a stop in a frame of its own.
    rig.pointer.scroll(smooth(3.0, 0.0)).unwrap();
    rig.fake.expect(&[
        ("absolute", Seen::Scroll(-3.0, 0.0)),
        ("absolute", Seen::Frame),
    ]);
    let mut end = smooth(0.0, 0.0);
    end.phase = ScrollPhase::Ended;
    rig.pointer.scroll(end).unwrap();
    rig.fake.expect(&[
        (
            "absolute",
            Seen::ScrollStop {
                x: true,
                y: true,
                cancel: false,
            },
        ),
        ("absolute", Seen::Frame),
    ]);
    // A cancelled gesture is a cancel.
    let mut cancelled = smooth(0.0, 2.0);
    cancelled.phase = ScrollPhase::Cancelled;
    rig.pointer.scroll(cancelled).unwrap();
    rig.fake.expect(&[
        ("absolute", Seen::Scroll(0.0, -2.0)),
        ("absolute", Seen::Frame),
        (
            "absolute",
            Seen::ScrollStop {
                x: true,
                y: true,
                cancel: true,
            },
        ),
        ("absolute", Seen::Frame),
    ]);
    // release_all lets go of every device that was emulating.
    rig.pointer.button(MouseButton::PRIMARY, true).unwrap();
    rig.fake.expect(&[
        ("absolute", Seen::Button(0x110, true)),
        ("absolute", Seen::Frame),
    ]);
    rig.pointer.release_all().unwrap();
    rig.fake.expect(&[
        ("absolute", Seen::Button(0x110, false)),
        ("absolute", Seen::Frame),
        ("pointer", Seen::Stop),
        ("absolute", Seen::Stop),
    ]);
    rig.fake.quiet();
}

#[test]
fn absolute_motion_is_global_logical_across_mixed_scales() {
    let mut rig = rig();
    // Display 1: scale 1, origin 0. Display 2: scale 2, origin x = 1920, so device pixel
    // (100, 50) is logical (1920 + 50, 25).
    rig.pointer
        .move_to(DisplayId(1), PointDevice::new(100.5, 50.25))
        .unwrap();
    rig.fake.expect(&[
        ("absolute", Seen::Start(1)),
        ("absolute", Seen::Motion(100.5, 50.25)),
        ("absolute", Seen::Frame),
    ]);
    rig.pointer
        .move_to(DisplayId(2), PointDevice::new(100.0, 50.0))
        .unwrap();
    rig.fake.expect(&[
        ("absolute", Seen::Motion(1970.0, 25.0)),
        ("absolute", Seen::Frame),
    ]);
    // The far corner of display 2 is still inside its region.
    rig.pointer
        .move_to(DisplayId(2), PointDevice::new(3839.0, 1439.0))
        .unwrap();
    rig.fake.expect(&[
        ("absolute", Seen::Motion(3839.5, 719.5)),
        ("absolute", Seen::Frame),
    ]);
    // Unknown display, off-display position, and a display with no region.
    assert!(matches!(
        rig.pointer
            .move_to(DisplayId(9), PointDevice::new(1.0, 1.0)),
        Err(PlatformError::NotFound)
    ));
    assert!(matches!(
        rig.pointer
            .move_to(DisplayId(1), PointDevice::new(1920.0, 1.0)),
        Err(PlatformError::Unsupported(_))
    ));
    rig.fake.quiet();
    let (_gate, source, _keys, mut pointer) =
        source_with(vec![display(3, (1920, 1080), 1.0, (9000.0, 0.0))]);
    let _other = Fake::connect(
        &source,
        vec![
            keyboard(None),
            absolute("absolute", vec![(0, 0, 1920, 1080)]),
        ],
    );
    assert!(matches!(
        pointer.move_to(DisplayId(3), PointDevice::new(5.0, 5.0)),
        Err(PlatformError::NotFound)
    ));
}

#[test]
fn one_absolute_device_per_monitor_is_chosen_by_region() {
    let mut rig = rig_with(
        vec![
            keyboard(None),
            absolute("left", vec![(0, 0, 1920, 1080)]),
            absolute("right", vec![(1920, 0, 1920, 720)]),
        ],
        two_displays(),
    );
    rig.pointer
        .move_to(DisplayId(2), PointDevice::new(100.0, 50.0))
        .unwrap();
    rig.fake.expect(&[
        ("right", Seen::Start(1)),
        ("right", Seen::Motion(1970.0, 25.0)),
        ("right", Seen::Frame),
    ]);
    rig.pointer
        .move_to(DisplayId(1), PointDevice::new(10.0, 10.0))
        .unwrap();
    rig.fake.expect(&[
        ("left", Seen::Start(2)),
        ("left", Seen::Motion(10.0, 10.0)),
        ("left", Seen::Frame),
    ]);
    // Buttons follow the device that moved last.
    rig.pointer.button(MouseButton::PRIMARY, true).unwrap();
    rig.fake
        .expect(&[("left", Seen::Button(0x110, true)), ("left", Seen::Frame)]);
    rig.pointer.release_all().unwrap();
    rig.fake.expect(&[
        ("left", Seen::Button(0x110, false)),
        ("left", Seen::Frame),
        ("left", Seen::Stop),
        ("right", Seen::Stop),
    ]);
}

#[test]
fn a_scroll_gesture_ends_on_the_device_that_holds_it() {
    let mut rig = rig_with(
        vec![
            keyboard(None),
            absolute("left", vec![(0, 0, 1920, 1080)]),
            absolute("right", vec![(1920, 0, 1920, 720)]),
        ],
        two_displays(),
    );
    rig.pointer
        .move_to(DisplayId(1), PointDevice::new(10.0, 10.0))
        .unwrap();
    rig.pointer.scroll(smooth(0.0, 4.0)).unwrap();
    rig.fake.expect(&[
        ("left", Seen::Start(1)),
        ("left", Seen::Motion(10.0, 10.0)),
        ("left", Seen::Frame),
        ("left", Seen::Scroll(0.0, -4.0)),
        ("left", Seen::Frame),
    ]);
    // The pointer moves to the other monitor's device, then the gesture ends (a macOS momentum
    // tail can outlive the pointer position it began at).
    rig.pointer
        .move_to(DisplayId(2), PointDevice::new(100.0, 50.0))
        .unwrap();
    rig.fake.expect(&[
        ("right", Seen::Start(2)),
        ("right", Seen::Motion(1970.0, 25.0)),
        ("right", Seen::Frame),
    ]);
    let mut end = smooth(0.0, 0.0);
    end.phase = ScrollPhase::MomentumEnded;
    rig.pointer.scroll(end).unwrap();
    rig.fake.expect(&[
        (
            "left",
            Seen::ScrollStop {
                x: true,
                y: true,
                cancel: false,
            },
        ),
        ("left", Seen::Frame),
    ]);
    // Nothing is left in progress: release_all only settles the devices.
    rig.pointer.release_all().unwrap();
    rig.fake
        .expect(&[("left", Seen::Stop), ("right", Seen::Stop)]);
    rig.fake.quiet();
}

#[test]
fn lock_keys_follow_the_compositor_and_taps_toggle() {
    let Some(text) = test_keymap_text() else {
        eprintln!("skipped: the xkb data files are not installed");
        return;
    };
    let mut rig = rig_with(
        vec![
            keyboard(Some(text)),
            absolute("absolute", vec![(0, 0, 1920, 1080)]),
        ],
        two_displays(),
    );
    assert_eq!(rig.keys.lock_keys().unwrap(), LockKeys::default());
    // Caps Lock (modifier 1) on.
    rig.fake.send(Ctl::Modifiers {
        device: "keyboard",
        locked: 1 << 1,
    });
    wait_until("caps lock to be reported", || {
        rig.keys.lock_keys().unwrap().caps_lock == Some(true)
    });
    assert_eq!(rig.keys.lock_keys().unwrap().num_lock, Some(false));
    assert_eq!(rig.keys.lock_keys().unwrap().scroll_lock, None);
    // Already as wanted, and "leave unchanged": nothing is sent.
    rig.keys
        .set_lock_keys(LockKeys {
            caps_lock: Some(true),
            num_lock: None,
            scroll_lock: None,
        })
        .unwrap();
    rig.keys.set_lock_keys(LockKeys::default()).unwrap();
    rig.fake.quiet();
    // Num Lock off to on: one tap, down and up each in a frame of their own.
    rig.keys
        .set_lock_keys(LockKeys {
            caps_lock: None,
            num_lock: Some(true),
            scroll_lock: None,
        })
        .unwrap();
    rig.fake.expect(&[
        ("keyboard", Seen::Start(1)),
        ("keyboard", Seen::Key(69, true)),
        ("keyboard", Seen::Frame),
        ("keyboard", Seen::Key(69, false)),
        ("keyboard", Seen::Frame),
    ]);
    assert_eq!(rig.keys.lock_keys().unwrap().num_lock, Some(true));
    // The compositor's own report replaces the guess.
    rig.fake.send(Ctl::Modifiers {
        device: "keyboard",
        locked: (1 << 1) | (1 << 4),
    });
    rig.fake.send(Ctl::Modifiers {
        device: "keyboard",
        locked: 0,
    });
    wait_until("both locks to be reported off", || {
        let locks = rig.keys.lock_keys().unwrap();
        (locks.caps_lock, locks.num_lock) == (Some(false), Some(false))
    });
    // A lock key held by us isn't tapped.
    rig.keys.key(CAPS, true).unwrap();
    rig.fake
        .expect(&[("keyboard", Seen::Key(58, true)), ("keyboard", Seen::Frame)]);
    rig.keys
        .set_lock_keys(LockKeys {
            caps_lock: Some(true),
            num_lock: None,
            scroll_lock: None,
        })
        .unwrap();
    rig.fake.quiet();
}

#[test]
fn pausing_a_device_resets_its_holds_and_the_source_is_not_live_until_it_resumes() {
    let mut rig = rig();
    rig.keys.key(KEY_A, true).unwrap();
    for _ in 0..3 {
        rig.fake.next();
    }
    rig.fake.send(Ctl::Pause("keyboard"));
    wait_until("the source to stop being live", || !rig.source.is_live());
    match rig.keys.key(KEY_B, true) {
        Err(PlatformError::Backend(message)) => assert_eq!(message, "no portal input session"),
        other => panic!("{other:?}"),
    }
    // The compositor reset the keyboard: nothing is owed, nothing is sent.
    rig.keys.release_all().unwrap();
    rig.fake.quiet();
    rig.fake.send(Ctl::Resume("keyboard"));
    wait_until("the source to be live again", || rig.source.is_live());
    // A is no longer held, so pressing it is a new down, with a new emulation sequence.
    rig.keys.key(KEY_A, true).unwrap();
    rig.fake.expect(&[
        ("keyboard", Seen::Start(2)),
        ("keyboard", Seen::Key(30, true)),
        ("keyboard", Seen::Frame),
    ]);
}

#[test]
fn a_new_attach_releases_what_the_old_connection_held_then_closes_it() {
    let mut rig = rig();
    rig.keys.key(KEY_A, true).unwrap();
    rig.pointer.button(MouseButton::PRIMARY, true).unwrap();
    for _ in 0..3 + 3 {
        rig.fake.next();
    }
    let second = Fake::connect(
        &rig.source,
        vec![
            keyboard(None),
            absolute("absolute", vec![(0, 0, 1920, 1080)]),
        ],
    );
    // Keys first, then buttons, each device stopped once nothing is left, then goodbye.
    rig.fake.expect(&[
        ("keyboard", Seen::Key(30, false)),
        ("keyboard", Seen::Frame),
        ("keyboard", Seen::Stop),
        ("pointer", Seen::Button(0x110, false)),
        ("pointer", Seen::Frame),
        ("pointer", Seen::Stop),
        ("", Seen::Disconnect),
    ]);
    // The ledger started over: A is a fresh down on the new connection.
    rig.keys.key(KEY_A, true).unwrap();
    second.expect(&[
        ("keyboard", Seen::Start(1)),
        ("keyboard", Seen::Key(30, true)),
        ("keyboard", Seen::Frame),
    ]);
}

#[test]
fn detach_and_dropping_the_handles_release_and_disconnect() {
    let rig = rig();
    let Rig {
        source,
        mut keys,
        pointer,
        fake,
        ..
    } = rig;
    keys.key(KEY_A, true).unwrap();
    for _ in 0..3 {
        fake.next();
    }
    source.detach();
    fake.expect(&[
        ("keyboard", Seen::Key(30, false)),
        ("keyboard", Seen::Frame),
        ("keyboard", Seen::Stop),
        ("", Seen::Disconnect),
    ]);
    wait_until("the source to stop being live", || !source.is_live());
    assert!(matches!(
        keys.key(KEY_A, true),
        Err(PlatformError::Backend(_))
    ));
    keys.release_all().unwrap();
    source.detach();

    // Dropping the key handle releases its keys; dropping the last handle disconnects.
    let again = Fake::connect(
        &source,
        vec![
            keyboard(None),
            absolute("absolute", vec![(0, 0, 1920, 1080)]),
        ],
    );
    keys.key(KEY_B, true).unwrap();
    for _ in 0..3 {
        again.next();
    }
    drop(keys);
    again.expect(&[
        ("keyboard", Seen::Key(48, false)),
        ("keyboard", Seen::Frame),
        ("keyboard", Seen::Stop),
    ]);
    drop(pointer);
    again.quiet();
    drop(source);
    again.expect(&[("", Seen::Disconnect)]);
}

#[test]
fn without_a_connection_presses_fail_and_releases_succeed() {
    let (_gate, source, mut keys, mut pointer) = source_with(two_displays());
    assert!(!source.is_live());
    match keys.key(KEY_A, true) {
        Err(PlatformError::Backend(message)) => assert_eq!(message, "no portal input session"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        pointer.button(MouseButton::PRIMARY, true),
        Err(PlatformError::Backend(_))
    ));
    assert!(matches!(
        pointer.scroll(wheel(0, 120)),
        Err(PlatformError::Backend(_))
    ));
    assert!(matches!(
        pointer.move_to(DisplayId(1), PointDevice::new(1.0, 1.0)),
        Err(PlatformError::Backend(_))
    ));
    keys.key(KEY_A, false).unwrap();
    keys.release_all().unwrap();
    keys.recover_keys(&[KEY_A, KEY_B]).unwrap();
    pointer.button(MouseButton::PRIMARY, false).unwrap();
    pointer.release_all().unwrap();
    pointer.recover_buttons(&[MouseButton::PRIMARY]).unwrap();
    // Ending a gesture that isn't running is nothing, with or without a session.
    let mut end = wheel(0, 0);
    end.phase = ScrollPhase::Ended;
    pointer.scroll(end).unwrap();
    assert_eq!(keys.lock_keys().unwrap(), LockKeys::default());
    source.detach();
}

#[test]
fn unmapped_input_is_refused_before_it_reaches_the_wire() {
    let mut rig = rig();
    let unmapped = HidUsage {
        page: 0x0c,
        id: 0xfff0,
    };
    match rig.keys.key(unmapped, true) {
        Err(PlatformError::Unsupported(message)) => {
            assert_eq!(message, "HID usage has no evdev code");
        }
        other => panic!("{other:?}"),
    }
    rig.keys.recover_keys(&[unmapped]).unwrap();
    assert!(matches!(
        rig.pointer.button(MouseButton(0), true),
        Err(PlatformError::Unsupported(_))
    ));
    assert!(matches!(
        rig.pointer.scroll(smooth(f64::NAN, 0.0)),
        Err(PlatformError::Unsupported(_))
    ));
    rig.fake.quiet();
}

#[test]
fn the_compositor_hanging_up_ends_the_connection_and_a_new_one_can_follow() {
    let mut rig = rig();
    rig.keys.key(KEY_A, true).unwrap();
    for _ in 0..3 {
        rig.fake.next();
    }
    rig.fake.send(Ctl::Hangup);
    wait_until("the source to notice", || !rig.source.is_live());
    // Nothing can be held on a connection that is gone.
    rig.keys.release_all().unwrap();
    assert!(matches!(
        rig.keys.key(KEY_A, true),
        Err(PlatformError::Backend(_))
    ));
    let next = Fake::connect(
        &rig.source,
        vec![
            keyboard(None),
            absolute("absolute", vec![(0, 0, 1920, 1080)]),
        ],
    );
    rig.keys.key(KEY_A, true).unwrap();
    next.expect(&[
        ("keyboard", Seen::Start(1)),
        ("keyboard", Seen::Key(30, true)),
        ("keyboard", Seen::Frame),
    ]);
}

#[test]
fn recovery_releases_only_what_this_source_holds() {
    let mut rig = rig();
    rig.keys.key(KEY_A, true).unwrap();
    rig.keys.key(KEY_B, true).unwrap();
    rig.pointer.button(MouseButton::PRIMARY, true).unwrap();
    for _ in 0..5 + 3 {
        rig.fake.next();
    }
    // B is up already in the journal's view; C was never pressed here.
    rig.keys
        .recover_keys(&[KEY_A, HidUsage::keyboard(0x06)])
        .unwrap();
    rig.fake.expect(&[
        ("keyboard", Seen::Key(30, false)),
        ("keyboard", Seen::Frame),
    ]);
    rig.keys.recover_keys(&[KEY_B]).unwrap();
    rig.fake.expect(&[
        ("keyboard", Seen::Key(48, false)),
        ("keyboard", Seen::Frame),
        ("keyboard", Seen::Stop),
    ]);
    rig.pointer
        .recover_buttons(&[MouseButton::PRIMARY, MouseButton::SECONDARY])
        .unwrap();
    rig.fake.expect(&[
        ("pointer", Seen::Button(0x110, false)),
        ("pointer", Seen::Frame),
        ("pointer", Seen::Stop),
    ]);
    rig.fake.quiet();
}
