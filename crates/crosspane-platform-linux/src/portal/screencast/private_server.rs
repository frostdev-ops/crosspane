//! Test-only: the capture's PipeWire data path against a private PipeWire server.
//!
//! The test starts its own `pipewire` daemon (a throwaway runtime directory, no session bus, no
//! device modules, a dummy driver and the link factory) and gives every PipeWire client in this
//! process one connection to it through `connect_fd`; nothing here ever connects to a default
//! PipeWire socket, so the desktop's own server is out of reach. On it run
//!
//! - a **producer**: a `Video/Source` stream that offers BGRx at 64x48, allocates memory-backed
//!   buffers with *padded* rows, and paints pixel (x, y) as `[x, y, 0x77, 0x00]` with the padding
//!   bytes `0xEE`;
//! - a **linker**: another client that waits for the producer's and the capture's nodes (there is
//!   no session manager to do it) and links them through the link factory;
//! - the **capture** under test: a `PipeWireThread` with an attached core and a started stream.
//!
//! That exercises what the fake portal cannot: format negotiation against a compliant producer,
//! the buffer and metadata parameters, memory mapping, stride and chunk handling, conversion,
//! cropping, pacing against a real clock, the gate, and the loss of a node. A producer is not a
//! compositor: mutter's and KWin's buffers, metadata and timing remain a live check. Tests skip
//! with a printed reason when there is no `pipewire` binary.

use std::collections::HashMap;
use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crosspane_platform::{EventSink, Frame, FrameEvent, IoGate, StreamEndReason, StreamId};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::euclid::point2;
use crosspane_types::geom::{DisplayGeometry, PixelRect, PixelSize, PointLogical, SizeMm};
use crosspane_types::id::DisplayId;
use pipewire as pw;
use pw::spa;
use spa::param::ParamType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::{self, ChoiceValue, Object, Pod, Property, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle, SpaTypes};

use super::capture::{Command, PipeWireThread, StartRequest};
use super::worker::Shared;

const WAIT: Duration = Duration::from_secs(10);
const PRODUCER: &str = "crosspane.test.producer";
const CAPTURE: &str = "crosspane-screencast";
const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
/// Row stride of the producer's buffers: 16 bytes of padding after each row.
const STRIDE: u32 = WIDTH * 4 + 16;
/// `dataType` masks a producer may allow.
const MEMFD: i32 = 1 << spa::sys::SPA_DATA_MemFd;
const MEMPTR: i32 = 1 << spa::sys::SPA_DATA_MemPtr;
/// The red byte of every pixel the producer paints.
const RED: u8 = 0x77;

// ---- the private server ------------------------------------------------------------------------

/// A `pipewire` daemon on a private socket, killed on drop.
struct Server {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
}

impl Server {
    fn start() -> Option<Server> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "cpg22-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        // Short: a unix socket path may not exceed 107 bytes.
        let dir = PathBuf::from("/tmp").join(&name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let config = dir.join("private.conf");
        fs::write(
            &config,
            format!(
                "context.properties = {{\n\
                 \x20   core.daemon = true\n\
                 \x20   core.name = \"{name}\"\n\
                 \x20   support.dbus = false\n\
                 \x20   default.clock.rate = 48000\n\
                 \x20   default.clock.quantum = 1024\n\
                 \x20   default.clock.min-quantum = 1024\n\
                 \x20   default.clock.max-quantum = 1024\n\
                 }}\n\
                 context.spa-libs = {{\n\
                 \x20   support.* = support/libspa-support\n\
                 }}\n\
                 context.modules = [\n\
                 \x20   {{ name = libpipewire-module-protocol-native }}\n\
                 \x20   {{ name = libpipewire-module-access args = {{ access.force = unrestricted }} }}\n\
                 \x20   {{ name = libpipewire-module-spa-node-factory }}\n\
                 \x20   {{ name = libpipewire-module-client-node }}\n\
                 \x20   {{ name = libpipewire-module-link-factory }}\n\
                 ]\n\
                 context.objects = [\n\
                 \x20   {{ factory = spa-node-factory args = {{\n\
                 \x20       factory.name = support.node.driver\n\
                 \x20       node.name = crosspane.private.driver\n\
                 \x20       priority.driver = 20000\n\
                 \x20   }} }}\n\
                 ]\n"
            ),
        )
        .unwrap();
        let log = fs::File::create(dir.join("server.log")).unwrap();
        let child = match Process::new("pipewire")
            .arg("-c")
            .arg(&config)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &dir)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("PIPEWIRE_RUNTIME_DIR", &dir)
            .env("PIPEWIRE_REMOTE", &name)
            .env("PIPEWIRE_CONFIG_DIR", &dir)
            .env("PIPEWIRE_CONFIG_PREFIX", "")
            .env("PIPEWIRE_CONFIG_NAME", "private.conf")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("skipping: cannot run pipewire: {error}");
                let _ = fs::remove_dir_all(&dir);
                return None;
            }
        };
        let socket = dir.join(&name);
        let mut server = Server { child, dir, socket };
        let deadline = Instant::now() + WAIT;
        while !server.socket.exists() {
            if let Ok(Some(status)) = server.child.try_wait() {
                let log = fs::read_to_string(server.dir.join("server.log")).unwrap_or_default();
                eprintln!("skipping: the private pipewire exited ({status}): {log}");
                return None;
            }
            assert!(Instant::now() < deadline, "pipewire did not come up");
            thread::sleep(Duration::from_millis(5));
        }
        Some(server)
    }

    /// A new connection to the server, as the portal's `OpenPipeWireRemote` would hand over.
    fn connection(&self) -> OwnedFd {
        OwnedFd::from(UnixStream::connect(&self.socket).unwrap())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// ---- the producer ------------------------------------------------------------------------------

fn serialize(object: Object) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .unwrap()
    .0
    .into_inner()
}

struct ProducerState {
    size: Option<(usize, usize)>,
    painted: Arc<AtomicU64>,
}

/// A producer thread; dropping it stops it (and so removes its node).
struct Producer {
    stop: Arc<AtomicBool>,
    painted: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl Producer {
    /// `memory`: the `dataType` mask the producer allows.
    fn start(fd: OwnedFd, memory: i32) -> Producer {
        let stop = Arc::new(AtomicBool::new(false));
        let painted = Arc::new(AtomicU64::new(0));
        let thread = {
            let (stop, painted) = (Arc::clone(&stop), Arc::clone(&painted));
            thread::spawn(move || produce(fd, &stop, painted, memory))
        };
        Producer {
            stop,
            painted,
            thread: Some(thread),
        }
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.halt();
    }
}

fn produce(fd: OwnedFd, stop: &AtomicBool, painted: Arc<AtomicU64>, memory: i32) {
    let mainloop = pw::main_loop::MainLoopRc::new(None).unwrap();
    let context = pw::context::ContextRc::new(&mainloop, None).unwrap();
    let core = context.connect_fd_rc(fd, None).unwrap();
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        PRODUCER,
        pw::properties::properties! {
            *pw::keys::NODE_NAME => PRODUCER,
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .unwrap();
    let _listener = stream
        .add_local_listener_with_user_data(ProducerState {
            size: None,
            painted,
        })
        .param_changed(move |stream, state, id, pod| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(pod) = pod else { return };
            let mut info = VideoInfoRaw::new();
            if info.parse(pod).is_err() {
                return;
            }
            let (width, height) = (info.size().width as usize, info.size().height as usize);
            state.size = Some((width, height));
            let buffers = serialize(Object {
                type_: SpaTypes::ObjectParamBuffers.as_raw(),
                id: ParamType::Buffers.as_raw(),
                properties: vec![
                    Property::new(
                        spa::sys::SPA_PARAM_BUFFERS_buffers,
                        Value::Choice(ChoiceValue::Int(Choice(
                            ChoiceFlags::empty(),
                            ChoiceEnum::Range {
                                default: 4,
                                min: 2,
                                max: 8,
                            },
                        ))),
                    ),
                    Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
                    Property::new(
                        spa::sys::SPA_PARAM_BUFFERS_size,
                        Value::Int((STRIDE as usize * height) as i32),
                    ),
                    Property::new(
                        spa::sys::SPA_PARAM_BUFFERS_stride,
                        Value::Int(STRIDE as i32),
                    ),
                    Property::new(spa::sys::SPA_PARAM_BUFFERS_align, Value::Int(16)),
                    Property::new(
                        spa::sys::SPA_PARAM_BUFFERS_dataType,
                        Value::Choice(ChoiceValue::Int(Choice(
                            ChoiceFlags::empty(),
                            ChoiceEnum::Flags {
                                default: memory,
                                flags: Vec::new(),
                            },
                        ))),
                    ),
                ],
            });
            let header = serialize(Object {
                type_: SpaTypes::ObjectParamMeta.as_raw(),
                id: ParamType::Meta.as_raw(),
                properties: vec![
                    Property::new(
                        spa::sys::SPA_PARAM_META_type,
                        Value::Id(spa::utils::Id(spa::sys::SPA_META_Header)),
                    ),
                    Property::new(
                        spa::sys::SPA_PARAM_META_size,
                        Value::Int(size_of::<spa::sys::spa_meta_header>() as i32),
                    ),
                ],
            });
            let mut pods = [
                Pod::from_bytes(&buffers).unwrap(),
                Pod::from_bytes(&header).unwrap(),
            ];
            stream.update_params(&mut pods).unwrap();
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some((width, height)) = state.size else {
                return;
            };
            let [data] = buffer.datas_mut() else { return };
            let Some(bytes) = data.data() else { return };
            let stride = STRIDE as usize;
            if bytes.len() < stride * height {
                return;
            }
            for y in 0..height {
                let row = &mut bytes[y * stride..(y + 1) * stride];
                for x in 0..width {
                    row[x * 4..x * 4 + 4].copy_from_slice(&[x as u8, y as u8, RED, 0x00]);
                }
                row[width * 4..].fill(0xEE);
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.size_mut() = (stride * height) as u32;
            *chunk.stride_mut() = stride as i32;
            state.painted.fetch_add(1, Ordering::Relaxed);
        })
        .register()
        .unwrap();
    let format = serialize(pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
        pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: WIDTH,
                height: HEIGHT
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 30, denom: 1 }
        ),
    ));
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [Pod::from_bytes(&format).unwrap()],
        )
        .unwrap();
    while !stop.load(Ordering::Acquire) {
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
    }
}

// ---- the linker --------------------------------------------------------------------------------

/// Another client: learns the nodes' ids and, once both exist, links them.
struct Linker {
    nodes: Arc<Mutex<HashMap<String, u32>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Linker {
    fn start(fd: OwnedFd) -> Linker {
        let nodes = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (nodes, stop) = (Arc::clone(&nodes), Arc::clone(&stop));
            thread::spawn(move || link_when_ready(fd, &nodes, &stop))
        };
        Linker {
            nodes,
            stop,
            thread: Some(thread),
        }
    }

    fn node(&self, name: &str) -> Option<u32> {
        self.nodes.lock().unwrap().get(name).copied()
    }
}

impl Drop for Linker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn link_when_ready(fd: OwnedFd, nodes: &Arc<Mutex<HashMap<String, u32>>>, stop: &AtomicBool) {
    let mainloop = pw::main_loop::MainLoopRc::new(None).unwrap();
    let context = pw::context::ContextRc::new(&mainloop, None).unwrap();
    let core = context.connect_fd_rc(fd, None).unwrap();
    let registry = core.get_registry_rc().unwrap();
    let seen = Arc::clone(nodes);
    let gone = Arc::clone(nodes);
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ == pw::types::ObjectType::Node
                && let Some(props) = global.props
                && let Some(name) = props.get("node.name")
            {
                seen.lock().unwrap().insert(name.to_owned(), global.id);
            }
        })
        .global_remove(move |id| gone.lock().unwrap().retain(|_, node| *node != id))
        .register();
    let mut link: Option<pw::link::Link> = None;
    while !stop.load(Ordering::Acquire) {
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
        if link.is_none() {
            let (producer, capture) = {
                let nodes = nodes.lock().unwrap();
                (nodes.get(PRODUCER).copied(), nodes.get(CAPTURE).copied())
            };
            if let (Some(producer), Some(capture)) = (producer, capture) {
                link = core
                    .create_object::<pw::link::Link>(
                        "link-factory",
                        &pw::properties::properties! {
                            "link.output.node" => producer.to_string(),
                            "link.input.node" => capture.to_string(),
                            "object.linger" => "false",
                        },
                    )
                    .ok();
            }
        }
    }
}

// ---- the tests ---------------------------------------------------------------------------------

/// Records a stream's events.
#[derive(Default)]
struct Events(Mutex<Vec<FrameEvent>>);

impl EventSink<FrameEvent> for Events {
    fn send(&self, event: FrameEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Events {
    fn frames(&self) -> Vec<Frame> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                FrameEvent::Frame { frame, .. } => Some(frame.clone()),
                _ => None,
            })
            .collect()
    }

    fn ended(&self) -> Vec<StreamEndReason> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                FrameEvent::Ended { reason, .. } => Some(*reason),
                _ => None,
            })
            .collect()
    }
}

fn displays() -> super::DisplaysFn {
    Arc::new(|| {
        vec![DisplayInfo {
            id: DisplayId(1),
            name: "DP-1".to_owned(),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(600.0, 340.0),
                pixel_size: PixelSize::new(WIDTH, HEIGHT),
                scale: 1.0,
                logical_origin: PointLogical::new(0.0, 0.0),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }]
    })
}

/// The producer, the linker and the capture thread with its core attached. Fields drop in
/// order: the capture thread stops first, the server goes last.
struct Graph {
    _capture: PipeWireThread,
    commands: mpsc::Sender<Command>,
    linker: Linker,
    producer: Producer,
    gate: Arc<IoGate>,
    _server: Server,
}

impl Graph {
    /// A producer that, like a compositor, hands out memfd-backed buffers.
    fn start() -> Option<Graph> {
        Self::start_with(MEMFD)
    }

    fn start_with(memory: i32) -> Option<Graph> {
        let server = Server::start()?;
        let producer = Producer::start(server.connection(), memory);
        let linker = Linker::start(server.connection());
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let (commands, receiver) = mpsc::channel();
        let capture =
            PipeWireThread::spawn(receiver, Arc::clone(&gate), displays(), Shared::new()).unwrap();
        commands
            .send(Command::Attach {
                epoch: 1,
                fd: server.connection(),
            })
            .unwrap();
        Some(Graph {
            _capture: capture,
            commands,
            linker,
            producer,
            gate,
            _server: server,
        })
    }

    /// Start a capture stream on the producer's node.
    fn open(&self, crop: Option<PixelRect>, max_fps: u32, events: &Arc<Events>) -> StreamId {
        let node_id = wait_for_value("the producer's node", || self.linker.node(PRODUCER));
        let (reply, answer) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Start(Box::new(StartRequest {
                id: StreamId(1),
                epoch: 1,
                node_id,
                display: DisplayId(1),
                device_size: PixelSize::new(WIDTH, HEIGHT),
                crop,
                max_fps,
                sink: Arc::clone(events) as Arc<dyn EventSink<FrameEvent>>,
                cancelled: Arc::new(AtomicBool::new(false)),
                reply,
            })))
            .unwrap();
        answer.recv_timeout(WAIT).unwrap().unwrap();
        StreamId(1)
    }
}

fn wait_for_value<T>(what: &str, mut value: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = value() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    wait_for_value(what, || condition().then_some(()));
}

fn pixel(frame: &Frame, x: usize, y: usize) -> [u8; 4] {
    let (pixels, stride) = frame.cpu_pixels().unwrap();
    let at = y * stride as usize + x * 4;
    [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
}

/// The first frame of a producer that allows `memory`, checked pixel by pixel.
fn check_first_frame(memory: i32) {
    let Some(graph) = Graph::start_with(memory) else {
        return;
    };
    let events = Arc::new(Events::default());
    graph.open(None, 30, &events);
    wait_for("the first frame", || !events.frames().is_empty());
    let frame = events.frames()[0].clone();
    assert_eq!(frame.size, PixelSize::new(WIDTH, HEIGHT));
    // Tight rows, whatever the producer's padding, with every pixel where it belongs and an
    // opaque alpha byte (the producer sent 0x00).
    assert_eq!(frame.cpu_pixels().unwrap().1, WIDTH * 4);
    for (x, y) in [(0, 0), (1, 0), (0, 1), (63, 47), (17, 30), (63, 0), (0, 47)] {
        assert_eq!(
            pixel(&frame, x, y),
            [x as u8, y as u8, RED, 0xFF],
            "pixel ({x}, {y})"
        );
    }
    assert_eq!(frame.damage, None, "the first frame has no baseline");
    assert!(graph.producer.painted.load(Ordering::Relaxed) > 0);
}

#[test]
fn frames_flow_from_a_producer_with_memfd_buffers_and_padded_rows() {
    check_first_frame(MEMFD);
}

#[test]
fn frames_flow_from_a_producer_with_plain_memory_buffers() {
    check_first_frame(MEMPTR);
}

#[test]
fn a_dma_buf_only_producer_gives_no_frames() {
    let Some(graph) = Graph::start_with(1 << spa::sys::SPA_DATA_DmaBuf) else {
        return;
    };
    let events = Arc::new(Events::default());
    graph.open(None, 30, &events);
    thread::sleep(Duration::from_secs(2));
    // No buffer type in common: nothing flows, and nothing is invented. (The stream is given up
    // by the negotiation watchdog after `NEGOTIATION_TIMEOUT`, which this test does not wait for.)
    assert!(events.frames().is_empty());
    assert!(events.ended().is_empty());
}

#[test]
fn frames_are_cut_to_the_crop_and_the_crop_can_change_on_a_running_stream() {
    let Some(graph) = Graph::start() else {
        return;
    };
    let events = Arc::new(Events::default());
    let crop = PixelRect::new(point2(8, 4), point2(24, 12));
    let stream = graph.open(Some(crop), 30, &events);
    wait_for("a cropped frame", || !events.frames().is_empty());
    let frame = events.frames()[0].clone();
    assert_eq!(frame.size, PixelSize::new(16, 8));
    assert_eq!(pixel(&frame, 0, 0), [8, 4, 0x77, 0xFF]);
    assert_eq!(pixel(&frame, 15, 7), [23, 11, 0x77, 0xFF]);

    let (reply, answer) = mpsc::sync_channel(1);
    let wider = PixelRect::new(point2(0, 0), point2(32, 48));
    graph
        .commands
        .send(Command::SetCrop {
            stream,
            crop: Some(wider),
            reply,
        })
        .unwrap();
    answer.recv_timeout(WAIT).unwrap().unwrap();
    wait_for("a frame in the new crop", || {
        events
            .frames()
            .last()
            .is_some_and(|f| f.size == PixelSize::new(32, 48))
    });
    // A crop beyond the buffer is clamped to it, never fatal.
    let (reply, answer) = mpsc::sync_channel(1);
    graph
        .commands
        .send(Command::SetCrop {
            stream,
            crop: Some(PixelRect::new(point2(60, 40), point2(500, 500))),
            reply,
        })
        .unwrap();
    answer.recv_timeout(WAIT).unwrap().unwrap();
    wait_for("a clamped frame", || {
        events
            .frames()
            .last()
            .is_some_and(|f| f.size == PixelSize::new(4, 8))
    });
    assert!(events.ended().is_empty());
}

#[test]
fn a_slow_limit_holds_the_rate_down() {
    let Some(graph) = Graph::start() else {
        return;
    };
    let events = Arc::new(Events::default());
    graph.open(None, 5, &events);
    wait_for("the first frame", || !events.frames().is_empty());
    let started = Instant::now();
    let before = events.frames().len();
    thread::sleep(Duration::from_secs(2));
    let rate = (events.frames().len() - before) as f64 / started.elapsed().as_secs_f64();
    assert!(
        (3.0..=6.5).contains(&rate),
        "{rate:.1} frames per second under a limit of 5"
    );
    assert!(
        graph.producer.painted.load(Ordering::Relaxed) as f64 / started.elapsed().as_secs_f64()
            > 10.0,
        "the producer itself runs faster than the limit"
    );
}

#[test]
fn a_closed_gate_ends_the_stream_blocked_and_stops_the_frames() {
    let Some(graph) = Graph::start() else {
        return;
    };
    let events = Arc::new(Events::default());
    graph.open(None, 30, &events);
    wait_for("the first frame", || !events.frames().is_empty());
    graph.gate.set_engine_permits(false);
    wait_for("the end", || !events.ended().is_empty());
    assert_eq!(events.ended(), vec![StreamEndReason::Blocked]);
    let frames = events.frames().len();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(events.frames().len(), frames, "no frame after the end");
}

#[test]
fn a_producer_that_goes_away_ends_the_stream() {
    let Some(mut graph) = Graph::start() else {
        return;
    };
    let events = Arc::new(Events::default());
    graph.open(None, 30, &events);
    wait_for("the first frame", || !events.frames().is_empty());
    graph.producer.halt();
    wait_for("the end", || !events.ended().is_empty());
    // The display is still there, so the stream broke rather than the target left.
    assert_eq!(events.ended(), vec![StreamEndReason::Failed]);
}

#[test]
fn stopping_a_running_stream_ends_it_requested() {
    let Some(graph) = Graph::start() else {
        return;
    };
    let events = Arc::new(Events::default());
    let stream = graph.open(None, 30, &events);
    wait_for("the first frame", || !events.frames().is_empty());
    let (reply, answer) = mpsc::sync_channel(1);
    graph
        .commands
        .send(Command::Stop { stream, reply })
        .unwrap();
    answer.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(events.ended(), vec![StreamEndReason::Requested]);
    let frames = events.frames().len();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(events.frames().len(), frames);
}
