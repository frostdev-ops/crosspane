//! PipeWire audio adapter. Native objects and event delivery belong to one owned loop thread.
//!
//! Process callbacks run on that loop (RT_PROCESS is deliberately absent), so PipeWire's local
//! listener user data is never concurrently borrowed by process and format/state callbacks.
//! The callbacks themselves use only fixed byte copying, atomics and preallocated SPSC queues.
use crosspane_platform::{
    AudioCapture, AudioDeviceError, AudioEvent, AudioFormat, AudioHost, AudioKind, AudioPlayback,
    AudioStop, EventSink, IoGate, PlatformError, VirtualPorts,
};
use crosspane_types::id::NodeId;
use pipewire::{self as pw, spa};
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const REQUEST_TIME: Duration = Duration::from_secs(2);
const STOP_TIME: Duration = Duration::from_millis(50);
const TICK: Duration = Duration::from_millis(2);
const MAX_PHYSICAL: usize = 8;

fn backend_error(_: impl fmt::Display) -> PlatformError {
    // Arbitrary OS strings are deliberately excluded from errors and events.
    PlatformError::Backend("PipeWire audio operation failed".into())
}
fn ring(format: AudioFormat) -> Result<(Producer<f32>, Consumer<f32>), PlatformError> {
    if !format.is_valid() {
        return Err(PlatformError::Unsupported(
            "audio requires 48 kHz mono or stereo f32",
        ));
    }
    // Fifty milliseconds; no retained samples are transferred into a subsequent open.
    Ok(RingBuffer::new(format.frame_samples() * 5))
}
fn display_name(name: &str) -> String {
    let text: String = name
        .chars()
        .take(256)
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')'))
        .take(80)
        .collect();
    let text = text.trim();
    if text.is_empty() {
        "Peer".into()
    } else {
        text.into()
    }
}

#[derive(Default)]
struct StopState {
    disabled: AtomicBool,
    removed: AtomicBool,
    owner: Option<thread::ThreadId>,
    cancelled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    bad_buffer: AtomicBool,
    gate_closed: AtomicBool,
    unavailable_reported: AtomicBool,
}
impl StopState {
    fn disable(&self) {
        self.disabled.store(true, Ordering::Release);
    }
    fn permits(&self, gate: Option<&IoGate>) -> bool {
        if gate.is_some_and(|gate| !gate.is_open()) {
            self.gate_closed.store(true, Ordering::Release);
            self.disable();
        }
        if self.cancelled.load(Ordering::Acquire) || self.shutdown.load(Ordering::Acquire) {
            self.disable();
        }
        !self.disabled.load(Ordering::Acquire)
    }
}
struct StopHandle {
    state: Arc<StopState>,
    stopped: bool,
}
impl StopHandle {
    fn new(state: Arc<StopState>) -> Self {
        Self {
            state,
            stopped: false,
        }
    }
}
impl fmt::Debug for StopHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipeWireAudioStop")
            .field("disabled", &self.state.disabled.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}
impl AudioStop for StopHandle {
    fn stop(&mut self) {
        if std::mem::replace(&mut self.stopped, true) {
            return;
        }
        self.state.disable();
        if self.state.owner == Some(thread::current().id()) {
            return;
        }
        let deadline = Instant::now() + STOP_TIME.saturating_sub(Duration::from_millis(1));
        while !self.state.removed.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for StopHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

enum Request {
    Add(
        NodeId,
        String,
        SyncSender<Result<VirtualPorts, PlatformError>>,
    ),
    Remove(NodeId, SyncSender<Result<(), PlatformError>>),
    Subscribe(
        Arc<dyn EventSink<AudioEvent>>,
        SyncSender<Result<(), PlatformError>>,
    ),
    Capture(AudioFormat, SyncSender<Result<AudioCapture, PlatformError>>),
    Playback(
        AudioFormat,
        SyncSender<Result<AudioPlayback, PlatformError>>,
    ),
}
struct Command {
    request: Request,
    cancelled: Arc<AtomicBool>,
}

/// Linux AudioHost. Constructing it connects to PipeWire but creates no physical streams.
/// Production uses normal PipeWire connection/default selection. Tests must select a private
/// runtime and remote before construction; there is no fallback to a different server.
pub struct PipeWireAudioHost {
    commands: SyncSender<Command>,
    shutdown: Arc<AtomicBool>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    gate: Arc<IoGate>,
}
impl fmt::Debug for PipeWireAudioHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipeWireAudioHost").finish_non_exhaustive()
    }
}
impl PipeWireAudioHost {
    pub fn new(gate: Arc<IoGate>) -> Result<Self, PlatformError> {
        let started = Instant::now();
        let (commands, receiver) = mpsc::sync_channel(16);
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (done_tx, done) = mpsc::sync_channel(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let worker_gate = gate.clone();
        let thread = thread::Builder::new()
            .name("crosspane-pipewire".into())
            .spawn(move || {
                let _ = run(receiver, worker_shutdown, worker_gate, ready_tx);
                let _ = done_tx.send(());
            })
            .map_err(backend_error)?;
        match ready.recv_timeout(REQUEST_TIME.saturating_sub(started.elapsed())) {
            Ok(Ok(())) => Ok(Self {
                commands,
                shutdown,
                done,
                thread: Some(thread),
                gate,
            }),
            result => {
                shutdown.store(true, Ordering::Release);
                if done
                    .recv_timeout(REQUEST_TIME.saturating_sub(started.elapsed()))
                    .is_ok()
                {
                    let _ = thread.join();
                }
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(PlatformError::Timeout),
                }
            }
        }
    }
    fn request<T>(
        &self,
        build: impl FnOnce(SyncSender<Result<T, PlatformError>>) -> Request,
    ) -> Result<T, PlatformError> {
        let (reply, receive) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.commands
            .try_send(Command {
                request: build(reply),
                cancelled: cancelled.clone(),
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => PlatformError::Timeout,
                mpsc::TrySendError::Disconnected(_) => backend_error("disconnected"),
            })?;
        match receive.recv_timeout(REQUEST_TIME) {
            Ok(result) => result,
            Err(_) => {
                cancelled.store(true, Ordering::Release);
                Err(PlatformError::Timeout)
            }
        }
    }
}
impl AudioHost for PipeWireAudioHost {
    fn add_peer(&mut self, peer: NodeId, name: &str) -> Result<VirtualPorts, PlatformError> {
        let name = display_name(name);
        self.request(|reply| Request::Add(peer, name, reply))
    }
    fn remove_peer(&mut self, peer: NodeId) -> Result<(), PlatformError> {
        self.request(|reply| Request::Remove(peer, reply))
    }
    fn subscribe(&mut self, sink: Arc<dyn EventSink<AudioEvent>>) -> Result<(), PlatformError> {
        self.request(|reply| Request::Subscribe(sink, reply))
    }
    fn open_capture(&mut self, format: AudioFormat) -> Result<AudioCapture, PlatformError> {
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if !format.is_valid() {
            return Err(PlatformError::Unsupported("audio format"));
        }
        self.request(|reply| Request::Capture(format, reply))
    }
    fn open_playback(&mut self, format: AudioFormat) -> Result<AudioPlayback, PlatformError> {
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if !format.is_valid() {
            return Err(PlatformError::Unsupported("audio format"));
        }
        self.request(|reply| Request::Playback(format, reply))
    }
}
impl Drop for PipeWireAudioHost {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if self.done.recv_timeout(REQUEST_TIME).is_ok()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

// All of the following objects remain on the owned loop thread.
use std::{cell::RefCell, rc::Rc};
type Subscription = Rc<RefCell<Option<Arc<dyn EventSink<AudioEvent>>>>>;
fn event(subscription: &Subscription, event: AudioEvent) {
    if let Some(sink) = subscription.borrow().as_ref() {
        sink.send(event);
    }
}
enum Pcm {
    Receive(Producer<f32>),
    Send(Consumer<f32>),
}
struct Callback {
    pcm: Pcm,
    expected: AudioFormat,
    valid: bool,
    stop: Arc<StopState>,
    gate: Option<Arc<IoGate>>,
    peer: Option<NodeId>,
    kind: AudioKind,
    subscription: Subscription,
    active: Rc<AtomicBool>,
    buffers: Vec<u8>,
    buffer_count: usize,
}
impl Callback {
    fn report_failure(&self, error: AudioDeviceError) {
        if !self.stop.unavailable_reported.swap(true, Ordering::AcqRel) {
            event(
                &self.subscription,
                AudioEvent::DeviceError {
                    peer: self.peer,
                    kind: self.kind,
                    error,
                },
            );
        }
    }
    fn failed(&self, error: AudioDeviceError) {
        self.stop.disable();
        self.report_failure(error);
    }
    fn process(&mut self, stream: &pw::stream::Stream) {
        // Gate observation precedes dequeue, retaining capture or consuming playback. A closure
        // is latched permanently. The engine must also drop handles on transient closures.
        let permitted = self.stop.permits(self.gate.as_deref());
        let Some(mut buffer) = stream.dequeue_buffer() else {
            return;
        };
        let datas = buffer.datas_mut();
        if datas.len() != 1 {
            self.stop.bad_buffer.store(true, Ordering::Release);
            self.stop.disable();
            return;
        }
        let data = &mut datas[0];
        let frame_bytes = usize::from(self.expected.channels) * 4;
        match &mut self.pcm {
            Pcm::Receive(pcm) => {
                if !permitted || !self.valid {
                    return;
                }
                let offset = data.chunk().offset() as usize;
                let size = data.chunk().size() as usize;
                let stride = data.chunk().stride();
                let Some(bytes) = data.data() else {
                    self.stop.bad_buffer.store(true, Ordering::Release);
                    self.stop.disable();
                    return;
                };
                if stride != frame_bytes as i32
                    || size > self.expected.frame_samples() * 4
                    || !size.is_multiple_of(frame_bytes)
                    || offset.checked_add(size).is_none_or(|end| end > bytes.len())
                {
                    self.stop.bad_buffer.store(true, Ordering::Release);
                    self.stop.disable();
                    return;
                }
                for frame in bytes[offset..offset + size].chunks_exact(frame_bytes) {
                    // Reserve a whole interleaved frame. Dropping individual samples on overrun
                    // would rotate stereo channels when the consumer advances concurrently.
                    if pcm.slots() < usize::from(self.expected.channels) {
                        continue;
                    }
                    for sample in frame.as_chunks::<4>().0 {
                        let value = f32::from_le_bytes(*sample);
                        if self.stop.permits(self.gate.as_deref()) {
                            let _ = pcm.push(if value.is_finite() { value } else { 0.0 });
                        }
                    }
                }
            }
            Pcm::Send(pcm) => {
                let Some(bytes) = data.data() else {
                    self.stop.bad_buffer.store(true, Ordering::Release);
                    self.stop.disable();
                    return;
                };
                // Bound each buffer to one 10 ms graph quantum, even if PipeWire offers more.
                let size =
                    bytes.len().min(self.expected.frame_samples() * 4) / frame_bytes * frame_bytes;
                for frame in bytes[..size].chunks_exact_mut(frame_bytes) {
                    let ready = permitted
                        && self.valid
                        && pcm.slots() >= usize::from(self.expected.channels);
                    for sample in frame.as_chunks_mut::<4>().0 {
                        let value = if ready && self.stop.permits(self.gate.as_deref()) {
                            pcm.pop().unwrap_or(0.0)
                        } else {
                            0.0
                        };
                        *sample = if value.is_finite() { value } else { 0.0 }.to_le_bytes();
                    }
                }
                *data.chunk_mut().offset_mut() = 0;
                *data.chunk_mut().size_mut() = size as u32;
                *data.chunk_mut().stride_mut() = frame_bytes as i32;
            }
        }
    }
}
struct Removal(Arc<StopState>);
impl Drop for Removal {
    fn drop(&mut self) {
        self.0.removed.store(true, Ordering::Release);
    }
}
struct Endpoint<'a> {
    // Listener must disappear before its stream; stream before core/context/loop.
    _listener: pw::stream::StreamListener<Callback>,
    stream: pw::stream::StreamBox<'a>,
    stop: Arc<StopState>,
    active: Rc<AtomicBool>,
    peer: Option<NodeId>,
    kind: AudioKind,
    cancellation: Arc<AtomicBool>,
    _removed: Removal,
}
impl Drop for Endpoint<'_> {
    fn drop(&mut self) {
        self.stop.disable();
    }
}
fn endpoint<'a>(
    core: &'a pw::core::Core,
    subscription: &Subscription,
    mut data: Callback,
    name: &str,
    properties: pw::properties::PropertiesBox,
    cancellation: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> Result<Endpoint<'a>, PlatformError> {
    let state = Arc::get_mut(&mut data.stop).ok_or_else(|| backend_error("stop ownership"))?;
    state.cancelled = cancellation.clone();
    state.shutdown = shutdown;
    if !state.permits(data.gate.as_deref()) {
        return Err(if cancellation.load(Ordering::Acquire) {
            PlatformError::Timeout
        } else {
            PlatformError::Locked
        });
    }
    data.buffers = buffer_parameters(data.expected)?;
    let stream = pw::stream::StreamBox::new(core, name, properties).map_err(backend_error)?;
    let stop = data.stop.clone();
    let active = data.active.clone();
    let peer = data.peer;
    let kind = data.kind;
    let direction = match data.pcm {
        Pcm::Receive(_) => spa::utils::Direction::Input,
        Pcm::Send(_) => spa::utils::Direction::Output,
    };
    data.subscription = subscription.clone();
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(data.expected.rate);
    info.set_channels(u32::from(data.expected.channels));
    let listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, data, old, new| {
            let running = new == pw::stream::StreamState::Streaming;
            if running {
                data.stop
                    .unavailable_reported
                    .store(false, Ordering::Release);
            }
            if let Some(peer) = data.peer {
                if running != data.active.swap(running, Ordering::AcqRel) {
                    event(
                        &data.subscription,
                        AudioEvent::VirtualActive {
                            peer,
                            kind: data.kind,
                            active: running,
                        },
                    );
                }
            } else if old == pw::stream::StreamState::Streaming && !running {
                // The session manager may now relink the same admitted stream to the new
                // default. Report disruption without overriding normal default following.
                data.report_failure(AudioDeviceError::Unavailable);
            }
            if matches!(
                new,
                pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected
            ) {
                data.failed(AudioDeviceError::Unavailable);
            }
        })
        .param_changed(|stream, data, id, pod| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let was_valid = data.valid;
            data.valid = pod.is_some_and(|pod| {
                let mut info = spa::param::audio::AudioInfoRaw::new();
                info.parse(pod).is_ok()
                    && info.format() == spa::param::audio::AudioFormat::F32LE
                    && info.rate() == data.expected.rate
                    && info.channels() == u32::from(data.expected.channels)
            });
            if pod.is_some() && !data.valid || was_valid && !data.valid {
                data.failed(AudioDeviceError::Failed);
            } else if data.valid {
                if let Some(buffers) = spa::pod::Pod::from_bytes(&data.buffers) {
                    if stream.update_params(&mut [buffers]).is_err() {
                        data.failed(AudioDeviceError::Failed);
                    }
                } else {
                    data.failed(AudioDeviceError::Failed);
                }
            }
        })
        .add_buffer(|_, data, _| {
            data.buffer_count += 1;
            if data.buffer_count > 2 {
                data.failed(AudioDeviceError::Failed);
            }
        })
        .remove_buffer(|_, data, _| {
            data.buffer_count = data.buffer_count.saturating_sub(1);
        })
        .process(|stream, data| data.process(stream))
        .register()
        .map_err(backend_error)?;
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(backend_error)?
    .0
    .into_inner();
    let pod = spa::pod::Pod::from_bytes(&bytes).ok_or_else(|| backend_error("format"))?;
    let mut flags = pw::stream::StreamFlags::MAP_BUFFERS;
    if peer.is_none() {
        flags |= pw::stream::StreamFlags::AUTOCONNECT;
    }
    stream
        .connect(direction, None, flags, &mut [pod])
        .map_err(backend_error)?;
    Ok(Endpoint {
        _listener: listener,
        stream,
        _removed: Removal(stop.clone()),
        stop,
        active,
        peer,
        kind,
        cancellation,
    })
}
fn callback(
    pcm: Pcm,
    expected: AudioFormat,
    gate: Option<Arc<IoGate>>,
    peer: Option<NodeId>,
    kind: AudioKind,
    subscription: &Subscription,
) -> Callback {
    Callback {
        pcm,
        expected,
        valid: false,
        stop: Arc::new(StopState {
            owner: Some(thread::current().id()),
            ..StopState::default()
        }),
        gate,
        peer,
        kind,
        subscription: subscription.clone(),
        active: Rc::new(AtomicBool::new(false)),
        buffers: Vec::new(),
        buffer_count: 0,
    }
}
fn buffer_parameters(format: AudioFormat) -> Result<Vec<u8>, PlatformError> {
    use spa::pod::{Object, Property, Value};
    let object = Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_BUFFERS_buffers, Value::Int(2)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_size,
                Value::Int((format.frame_samples() * 4) as i32),
            ),
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_stride,
                Value::Int(i32::from(format.channels) * 4),
            ),
        ],
    };
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .map_err(backend_error)?
    .0
    .into_inner())
}
fn virtual_properties(peer: NodeId, name: &str, kind: AudioKind) -> pw::properties::PropertiesBox {
    let (suffix, label, class, position) = match kind {
        AudioKind::Speaker => ("speaker", "speakers", "Audio/Sink", "[ FL FR ]"),
        AudioKind::Microphone => ("mic", "microphone", "Audio/Source", "[ MONO ]"),
    };
    pw::properties::properties! {
        "node.name" => format!("crosspane.{peer}.{suffix}"),
        "node.description" => format!("{name} {label}"),
        "media.class" => class,
        "node.virtual" => "true",
        "node.pause-on-idle" => "true",
        "priority.session" => "0",
        "audio.rate" => "48000",
        "audio.channels" => kind.format().channels.to_string(),
        "audio.position" => position,
        "node.latency" => "480/48000",
        "node.max-latency" => "480/48000",
        "resample.disable" => "true",
        "adapter.auto-port-config" => "{ mode = dsp position = preserve }",
    }
}
fn physical_properties(format: AudioFormat, kind: AudioKind) -> pw::properties::PropertiesBox {
    // No target.object or node ID: selection and following remain with the normal session manager.
    pw::properties::properties! {
        "node.name" => if kind == AudioKind::Microphone { "crosspane.capture" } else { "crosspane.playback" },
        "media.type" => "Audio",
        "media.category" => if kind == AudioKind::Microphone { "Capture" } else { "Playback" },
        "media.role" => "Communication",
        "adapter.auto-port-config" => "{ mode = dsp position = preserve }",
        "audio.rate" => "48000",
        "audio.channels" => format.channels.to_string(),
        "audio.position" => if format.channels == 1 { "[ MONO ]" } else { "[ FL FR ]" },
        "node.latency" => "480/48000",
        "node.max-latency" => "480/48000",
        "resample.disable" => "true",
    }
}

fn run(
    receiver: Receiver<Command>,
    shutdown: Arc<AtomicBool>,
    gate: Arc<IoGate>,
    ready: SyncSender<Result<(), PlatformError>>,
) -> Result<(), PlatformError> {
    match worker(receiver, shutdown, gate, &ready) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = ready.try_send(Err(error));
            Err(backend_error("worker"))
        }
    }
}
fn worker(
    receiver: Receiver<Command>,
    shutdown: Arc<AtomicBool>,
    gate: Arc<IoGate>,
    ready: &SyncSender<Result<(), PlatformError>>,
) -> Result<(), PlatformError> {
    pw::init();
    let loop_ = pw::main_loop::MainLoopRc::new(None).map_err(backend_error)?;
    let context = pw::context::ContextRc::new(&loop_, None).map_err(backend_error)?;
    let core = context.connect_rc(None).map_err(backend_error)?;
    let failed = Rc::new(AtomicBool::new(false));
    let connected = Rc::new(AtomicBool::new(false));
    let connected_event = connected.clone();
    let failure_event = failed.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |_, _| {
            connected_event.store(true, Ordering::Release);
        })
        .error(move |_, _, _, _| {
            failure_event.store(true, Ordering::Release);
        })
        .register();
    core.sync(0).map_err(backend_error)?;
    let startup = Instant::now();
    while !connected.load(Ordering::Acquire) {
        if failed.load(Ordering::Acquire) {
            return Err(backend_error("connect"));
        }
        if shutdown.load(Ordering::Acquire) || startup.elapsed() >= REQUEST_TIME {
            return Err(PlatformError::Timeout);
        }
        loop_.loop_().iterate(pw::loop_::Timeout::Finite(TICK));
    }
    let subscription: Subscription = Rc::new(RefCell::new(None));
    let mut peers: HashMap<NodeId, [Endpoint<'_>; 2]> = HashMap::new();
    let mut physical: Vec<Endpoint<'_>> = Vec::new();
    // Removal of a physical link is a conservative device-switch/failure signal, even when the
    // replacement has identical format and never causes a stream-state transition. PipeWire may
    // reconnect the admitted stream while the gate stays open; we never pin a target object.
    let node_serials = Rc::new(RefCell::new(HashMap::<u32, u64>::new()));
    let serials_event = node_serials.clone();
    let links = Rc::new(RefCell::new(HashMap::<u32, ((u32, u64), (u32, u64))>::new()));
    let removed = Rc::new(RefCell::new(Vec::<((u32, u64), (u32, u64))>::new()));
    let links_add = links.clone();
    let links_remove = links.clone();
    let removed_event = removed.clone();
    let registry = core.get_registry().map_err(backend_error)?;
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ == pw::types::ObjectType::Node
                && let Some(props) = global.props
                && let Some(serial) = props
                    .get("object.serial")
                    .and_then(|serial| serial.parse::<u64>().ok())
            {
                serials_event.borrow_mut().insert(global.id, serial);
            }
            if global.type_ == pw::types::ObjectType::Link
                && let Some(props) = global.props
                && let (Some(input), Some(output)) =
                    (props.get("link.input.node"), props.get("link.output.node"))
                && let (Ok(input), Ok(output)) = (input.parse(), output.parse())
            {
                let serials = serials_event.borrow();
                if let (Some(&input_serial), Some(&output_serial)) =
                    (serials.get(&input), serials.get(&output))
                {
                    links_add
                        .borrow_mut()
                        .insert(global.id, ((input, input_serial), (output, output_serial)));
                }
            }
        })
        .global_remove(move |id| {
            if let Some(nodes) = links_remove.borrow_mut().remove(&id) {
                removed_event.borrow_mut().push(nodes);
            }
        })
        .register();
    ready.send(Ok(())).map_err(backend_error)?;
    while !shutdown.load(Ordering::Acquire) && !failed.load(Ordering::Acquire) {
        loop_.loop_().iterate(pw::loop_::Timeout::Finite(TICK));
        for (input, output) in removed.borrow_mut().drain(..) {
            for stream in &physical {
                let id = stream.stream.node_id();
                let identity = node_serials.borrow().get(&id).map(|serial| (id, *serial));
                if identity == Some(input) || identity == Some(output) {
                    if stream
                        .stop
                        .unavailable_reported
                        .swap(true, Ordering::AcqRel)
                    {
                        continue;
                    }
                    event(
                        &subscription,
                        AudioEvent::DeviceError {
                            peer: None,
                            kind: stream.kind,
                            error: AudioDeviceError::Unavailable,
                        },
                    );
                }
            }
        }
        physical.retain(|stream| {
            // Also check the gate while idle, when there may be no process callbacks.
            let retain =
                stream.stop.permits(Some(&gate)) && !stream.cancellation.load(Ordering::Acquire);
            if !retain {
                stream.stop.disable();
                if stream.stop.gate_closed.swap(false, Ordering::AcqRel) {
                    event(
                        &subscription,
                        AudioEvent::DeviceError {
                            peer: None,
                            kind: stream.kind,
                            error: AudioDeviceError::Locked,
                        },
                    );
                }
                if stream.stop.bad_buffer.swap(false, Ordering::AcqRel) {
                    event(
                        &subscription,
                        AudioEvent::DeviceError {
                            peer: None,
                            kind: stream.kind,
                            error: AudioDeviceError::Failed,
                        },
                    );
                }
                // Destruction below is synchronous on this loop; removed is set after retain drops.
            }
            retain
        });
        // A separate removed signal is set by the endpoint-owned guard below, after stream drop.
        peers.retain(|_, endpoints| {
            for endpoint in endpoints.iter() {
                if endpoint.stop.bad_buffer.swap(false, Ordering::AcqRel) {
                    event(
                        &subscription,
                        AudioEvent::DeviceError {
                            peer: endpoint.peer,
                            kind: endpoint.kind,
                            error: AudioDeviceError::Failed,
                        },
                    );
                }
            }
            !endpoints[0].cancellation.load(Ordering::Acquire)
        });
        for _ in 0..16 {
            let Ok(command) = receiver.try_recv() else {
                break;
            };
            if command.cancelled.load(Ordering::Acquire) {
                continue;
            }
            match command.request {
                Request::Subscribe(sink, reply) => {
                    if subscription.borrow().is_some() {
                        let _ = reply.send(Err(PlatformError::Unsupported(
                            "audio subscription already installed",
                        )));
                        continue;
                    }
                    for endpoints in peers.values() {
                        for endpoint in endpoints {
                            if let Some(peer) = endpoint.peer {
                                sink.send(AudioEvent::VirtualActive {
                                    peer,
                                    kind: endpoint.kind,
                                    active: endpoint.active.load(Ordering::Acquire),
                                });
                            }
                        }
                    }
                    *subscription.borrow_mut() = Some(sink);
                    let _ = reply.send(Ok(()));
                }
                Request::Remove(peer, reply) => {
                    if let Some(endpoints) = peers.remove(&peer) {
                        for endpoint in &endpoints {
                            if endpoint.active.load(Ordering::Acquire) {
                                event(
                                    &subscription,
                                    AudioEvent::VirtualActive {
                                        peer,
                                        kind: endpoint.kind,
                                        active: false,
                                    },
                                );
                            }
                        }
                        drop(endpoints);
                        let _ = reply.send(Ok(()));
                    } else {
                        let _ = reply.send(Err(PlatformError::NotFound));
                    }
                }
                Request::Add(peer, name, reply) => {
                    if peers.contains_key(&peer) {
                        let _ = reply.send(Err(PlatformError::Unsupported("duplicate audio peer")));
                        continue;
                    }
                    let result = (|| {
                        let (speaker, speaker_out) = ring(AudioKind::Speaker.format())?;
                        let (mic_in, mic) = ring(AudioKind::Microphone.format())?;
                        let speaker = endpoint(
                            &core,
                            &subscription,
                            callback(
                                Pcm::Receive(speaker),
                                AudioKind::Speaker.format(),
                                None,
                                Some(peer),
                                AudioKind::Speaker,
                                &subscription,
                            ),
                            "crosspane-speakers",
                            virtual_properties(peer, &name, AudioKind::Speaker),
                            command.cancelled.clone(),
                            shutdown.clone(),
                        )?;
                        let mic = endpoint(
                            &core,
                            &subscription,
                            callback(
                                Pcm::Send(mic),
                                AudioKind::Microphone.format(),
                                None,
                                Some(peer),
                                AudioKind::Microphone,
                                &subscription,
                            ),
                            "crosspane-microphone",
                            virtual_properties(peer, &name, AudioKind::Microphone),
                            command.cancelled.clone(),
                            shutdown.clone(),
                        )?;
                        peers.insert(peer, [speaker, mic]);
                        Ok(VirtualPorts {
                            speaker_out,
                            mic_in,
                        })
                    })();
                    if reply.send(result).is_err() {
                        peers.remove(&peer);
                    }
                }
                Request::Capture(format, reply) => {
                    let result = (|| {
                        if !gate.is_open() {
                            return Err(PlatformError::Locked);
                        }
                        if physical.len() >= MAX_PHYSICAL {
                            return Err(PlatformError::Unsupported("audio stream limit"));
                        }
                        let (pcm, receive) = ring(format)?;
                        let stream = endpoint(
                            &core,
                            &subscription,
                            callback(
                                Pcm::Receive(pcm),
                                format,
                                Some(gate.clone()),
                                None,
                                AudioKind::Microphone,
                                &subscription,
                            ),
                            "crosspane-capture",
                            physical_properties(format, AudioKind::Microphone),
                            command.cancelled.clone(),
                            shutdown.clone(),
                        )?;
                        let handle = AudioCapture::new(
                            receive,
                            Box::new(StopHandle::new(stream.stop.clone())),
                        );
                        physical.push(stream);
                        Ok(handle)
                    })();
                    // On failure to deliver, disable before dropping the handle on our own thread.
                    if let Err(error) = reply.send(result) {
                        command.cancelled.store(true, Ordering::Release);
                        drop(error);
                    }
                }
                Request::Playback(format, reply) => {
                    let result = (|| {
                        if !gate.is_open() {
                            return Err(PlatformError::Locked);
                        }
                        if physical.len() >= MAX_PHYSICAL {
                            return Err(PlatformError::Unsupported("audio stream limit"));
                        }
                        let (send, pcm) = ring(format)?;
                        let stream = endpoint(
                            &core,
                            &subscription,
                            callback(
                                Pcm::Send(pcm),
                                format,
                                Some(gate.clone()),
                                None,
                                AudioKind::Speaker,
                                &subscription,
                            ),
                            "crosspane-playback",
                            physical_properties(format, AudioKind::Speaker),
                            command.cancelled.clone(),
                            shutdown.clone(),
                        )?;
                        let handle = AudioPlayback::new(
                            send,
                            Box::new(StopHandle::new(stream.stop.clone())),
                        );
                        physical.push(stream);
                        Ok(handle)
                    })();
                    if let Err(error) = reply.send(result) {
                        command.cancelled.store(true, Ordering::Release);
                        drop(error);
                    }
                }
            }
        }
    }
    if failed.load(Ordering::Acquire) {
        for endpoints in peers.values() {
            for endpoint in endpoints {
                event(
                    &subscription,
                    AudioEvent::DeviceError {
                        peer: endpoint.peer,
                        kind: endpoint.kind,
                        error: AudioDeviceError::Unavailable,
                    },
                );
            }
        }
        for endpoint in &physical {
            event(
                &subscription,
                AudioEvent::DeviceError {
                    peer: None,
                    kind: endpoint.kind,
                    error: AudioDeviceError::Unavailable,
                },
            );
        }
    }
    physical.clear();
    peers.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn format_and_ring_are_bounded() {
        for channels in [1, 2] {
            let format = AudioFormat {
                rate: 48_000,
                channels,
            };
            let (mut producer, mut consumer) = ring(format).unwrap();
            for _ in 0..format.frame_samples() * 5 {
                producer.push(0.25).unwrap();
            }
            assert!(producer.push(0.25).is_err());
            assert!(!format!("{producer:?}").contains("0.25"));
            for _ in 0..format.frame_samples() * 5 {
                assert!(consumer.pop() == Ok(0.25));
            }
            assert!(consumer.pop().is_err());
        }
        assert!(
            ring(AudioFormat {
                rate: 44_100,
                channels: 2
            })
            .is_err()
        );
        assert!(
            ring(AudioFormat {
                rate: 48_000,
                channels: 3
            })
            .is_err()
        );
    }
    #[test]
    fn timed_out_open_is_cancelled_without_a_server() {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let (commands, receiver) = mpsc::sync_channel(16);
        let (done_tx, done) = mpsc::sync_channel(1);
        let mut host = PipeWireAudioHost {
            commands,
            shutdown: Arc::new(AtomicBool::new(false)),
            done,
            thread: None,
            gate,
        };
        assert!(matches!(
            host.open_capture(AudioKind::Microphone.format()),
            Err(PlatformError::Timeout)
        ));
        let command = receiver.recv().unwrap();
        assert!(command.cancelled.load(Ordering::Acquire));
        // A late backend open observes the same cancellation atomic in every callback.
        let state = StopState {
            cancelled: command.cancelled,
            ..StopState::default()
        };
        assert!(!state.permits(None));
        done_tx.send(()).unwrap();
    }
    #[test]
    fn stop_and_gate_latch_without_a_server() {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let state = Arc::new(StopState::default());
        assert!(state.permits(Some(&gate)));
        gate.set_engine_permits(false);
        assert!(!state.permits(Some(&gate)));
        gate.set_engine_permits(true);
        assert!(!state.permits(Some(&gate)));
        state.removed.store(true, Ordering::Release);
        let mut stop = StopHandle::new(state.clone());
        stop.stop();
        stop.stop();
        drop(stop);
        assert!(state.disabled.load(Ordering::Acquire));
        // No loop ever acknowledges removal here. Exercise explicit stop and both public
        // handle destructors, including their subsequent boxed StopHandle destructor.
        for case in 0..3 {
            let state = Arc::new(StopState::default());
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let observer_state = state.clone();
            let observer_barrier = barrier.clone();
            let observer = thread::spawn(move || {
                observer_barrier.wait();
                let start = Instant::now();
                while !observer_state.disabled.load(Ordering::Acquire) {
                    assert!(start.elapsed() < STOP_TIME + Duration::from_millis(40));
                    thread::sleep(Duration::from_millis(1));
                }
                start.elapsed()
            });
            let stop = StopHandle::new(state.clone());
            let (producer, consumer) = ring(AudioKind::Microphone.format()).unwrap();
            // Construct public handles before timing: allocation/setup is not stop work.
            let action: Box<dyn FnOnce()> = match case {
                0 => Box::new(move || {
                    let mut stop = stop;
                    stop.stop();
                    stop.stop();
                    drop(stop);
                }),
                1 => {
                    let capture = AudioCapture::new(consumer, Box::new(stop));
                    Box::new(move || drop(capture))
                }
                _ => {
                    let playback = AudioPlayback::new(producer, Box::new(stop));
                    Box::new(move || drop(playback))
                }
            };
            barrier.wait();
            let start = Instant::now();
            action();
            let elapsed = start.elapsed();
            let disable_time = observer.join().unwrap();
            assert!(state.disabled.load(Ordering::Acquire));
            assert!(!state.removed.load(Ordering::Acquire));
            assert!(
                disable_time < STOP_TIME,
                "disable was delayed: {disable_time:?}"
            );
            // Forty milliseconds of scheduler margin over one 50 ms budget, while still
            // detecting the approximately 100 ms regression from waiting twice on Drop.
            assert!(
                elapsed < STOP_TIME + Duration::from_millis(40),
                "stop/Drop spent more than one wait budget: {elapsed:?}"
            );
        }
        let state = StopState::default();
        state.cancelled.store(true, Ordering::Release);
        assert!(!state.permits(None));
        let state = StopState::default();
        state.shutdown.store(true, Ordering::Release);
        assert!(!state.permits(None));
    }
}
