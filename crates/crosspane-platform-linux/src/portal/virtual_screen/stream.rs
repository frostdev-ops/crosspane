//! The PipeWire side of the virtual screen (WP-G2.4, task A).
//!
//! **One stream, many captures.** One thread (`crosspane-pipewire-virtual`) owns the PipeWire main
//! loop, its context, the core connected to the portal's restricted remote and **one** stream (the
//! *feed*), connected to the virtual source's node. The feed offers exactly one size
//! ([`format::enum_format`]); Mutter creates the virtual monitor when the format is negotiated and
//! gives it that size, and keeps it as long as the feed is connected. `resize` replaces the offer
//! (`update_params`) and the stream renegotiates.
//!
//! Every `FrameCapture::start` adds a [`Capture`] (the monitor capture's per-stream state:
//! crop, damage, pacing, the newest-wins slot, the closed-gate rule) to the feed. A buffer is read
//! and converted to tight BGRA **once** and handed to every capture; each cuts its own crop and
//! paces to its own `max_fps`. The feed also keeps the newest whole image, so a capture started on
//! a screen that nobody is repainting still gets a first frame at once (a compositor only sends
//! buffers on damage, and the feed's single consumer connected long before). That image is
//! converted for every buffer even with no capture running, and dropped when the gate closes.
//!
//! **Endings.** A closed gate ends every capture `Blocked` and leaves the screen alone (nothing is
//! read or converted until it opens again). The feed failing, its node going away, or the core
//! breaking is the screen's loss: the captures end `TargetGone` and the worker is told
//! (`Shared::pw_failed`). `Detach` (the worker, when the screen is over) ends them with the
//! reason the worker names.
//!
//! The thread, its commands, the callbacks that only mark faults and the loop that acts on them,
//! the destruction order (listener before stream, stream before core, core before context before
//! main loop) and the rules for sinks (called on this thread; they must not block or call back)
//! are those of `screencast::capture`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crosspane_platform::{EventSink, FrameEvent, IoGate, PlatformError, StreamEndReason, StreamId};
use crosspane_types::geom::{PixelRect, PixelSize};
use pipewire as pw;
use pw::spa;
use pw::stream::{StreamFlags, StreamState};
use spa::buffer::meta::{MetaHeader, MetaHeaderFlags};
use spa::param::ParamType;
use spa::pod::Pod;

use super::format;
use super::worker::Shared;
use crate::portal::screencast::capture::{
    BufferFault, Capture, Fault, buffer_damage, emit, read_image, send_params,
};
use crate::portal::screencast::format::{self as pods, FormatError, Negotiated};
use crate::portal::screencast::frames::{frame_time, mono_now, validate_crop};

/// How long the loop waits while the feed runs and nothing is due: the gate is polled at this rate.
const TICK: Duration = Duration::from_millis(10);
/// How long it waits with no feed: commands and shutdown are noticed at this rate.
const IDLE_TICK: Duration = Duration::from_millis(50);
/// The shortest wait (a zero timeout would spin).
const MIN_WAIT: Duration = Duration::from_millis(1);
/// How long `spawn` waits for the loop to come up.
const READY_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long dropping waits for the thread before detaching it.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// Commands handled per loop iteration.
const COMMANDS_PER_TICK: usize = 16;
/// The PipeWire node name of the feed.
pub(super) const STREAM_NAME: &str = "crosspane-virtual-screen";

fn backend(error: impl std::fmt::Display) -> PlatformError {
    PlatformError::Backend(format!("virtual screen: {error}"))
}

fn backend_format(error: FormatError) -> PlatformError {
    match error {
        FormatError::BadSize => backend("size out of range"),
        other => backend(format!("cannot build the stream parameters ({other:?})")),
    }
}

/// What the rest of the crate asks of the PipeWire thread.
pub(super) enum Command {
    /// The portal's PipeWire remote: connect to it.
    Attach {
        fd: OwnedFd,
    },
    /// Connect the feed to `node_id`, offering exactly `size`.
    Open {
        node_id: u32,
        size: PixelSize,
    },
    /// Offer exactly `size` instead; the stream renegotiates.
    Resize {
        size: PixelSize,
        reply: SyncSender<Result<(), PlatformError>>,
    },
    Start(Box<StartRequest>),
    SetCrop {
        stream: StreamId,
        crop: Option<PixelRect>,
        reply: SyncSender<Result<(), PlatformError>>,
    },
    /// End a capture with `Requested`. Unknown captures are fine (idempotent).
    Stop {
        stream: StreamId,
        reply: SyncSender<Result<(), PlatformError>>,
    },
    /// The screen is over: end every capture with `reason`, drop the feed and the connection.
    Detach {
        reason: StreamEndReason,
    },
}

/// A request to add a capture.
pub(super) struct StartRequest {
    pub id: StreamId,
    pub crop: Option<PixelRect>,
    pub max_fps: u32,
    pub sink: Arc<dyn EventSink<FrameEvent>>,
    /// Set by the caller when it stopped waiting; a request that finds it set is dropped.
    pub cancelled: Arc<AtomicBool>,
    pub reply: SyncSender<Result<(), PlatformError>>,
}

// ---- the feed's state --------------------------------------------------------------------------

/// The newest whole image as tight BGRA.
#[derive(Clone)]
struct Latest {
    size: PixelSize,
    pixels: Arc<[u8]>,
}

/// Why the feed is over, decided in a callback and acted on by the loop.
#[derive(Debug)]
enum Over {
    /// The stream failed.
    Failed(String),
    /// The stream or its node went away.
    Gone,
}

/// The feed's state. Touched by its callbacks and by the loop, never concurrently.
struct FeedState {
    /// The size the stream is currently asked for.
    wanted: PixelSize,
    /// The `Buffers` and `Meta` pods that answer a negotiated format.
    params: Vec<Vec<u8>>,
    negotiated: Option<Negotiated>,
    captures: HashMap<StreamId, Capture>,
    latest: Option<Latest>,
    over: Option<Over>,
    /// The stream has been connected to its node at some point.
    connected: bool,
    gate: Arc<IoGate>,
    shared: Arc<Shared>,
}

impl FeedState {
    fn mark(&mut self, over: Over) {
        self.over.get_or_insert(over);
    }

    /// Forget every pixel: the next frame of every capture is whole.
    fn forget_all(&mut self) {
        self.latest = None;
        for capture in self.captures.values_mut() {
            capture.forget_pixels();
        }
    }

    fn on_state(&mut self, new: &StreamState) {
        match new {
            StreamState::Error(message) => {
                tracing::warn!(%message, "the virtual screen stream failed");
                self.mark(Over::Failed(message.to_string()));
            }
            StreamState::Unconnected if self.connected => self.mark(Over::Gone),
            StreamState::Paused | StreamState::Streaming => self.connected = true,
            StreamState::Unconnected | StreamState::Connecting => {}
        }
    }

    /// The stream's format changed. Returns the parameters to answer with, if the format is usable.
    fn on_format(&mut self, pod: Option<&Pod>) -> Option<Vec<Vec<u8>>> {
        let Some(pod) = pod else {
            // Cleared: the stream is renegotiating (or stopping).
            self.negotiated = None;
            self.shared.set_negotiated(None);
            self.forget_all();
            return None;
        };
        match pods::parse_negotiated(pod) {
            Ok(negotiated) => {
                if self.negotiated != Some(negotiated) {
                    self.forget_all();
                    tracing::debug!(
                        format = ?negotiated.format,
                        width = negotiated.size.width,
                        height = negotiated.size.height,
                        "the virtual screen negotiated its format"
                    );
                    if negotiated.size != self.wanted {
                        tracing::warn!(
                            width = negotiated.size.width,
                            height = negotiated.size.height,
                            wanted_width = self.wanted.width,
                            wanted_height = self.wanted.height,
                            "the virtual screen stream settled on another size than offered"
                        );
                    }
                }
                self.negotiated = Some(negotiated);
                self.shared.set_negotiated(Some(negotiated.size));
                Some(self.params.clone())
            }
            Err(error) => {
                tracing::warn!(?error, "unusable virtual screen format");
                self.negotiated = None;
                self.shared.set_negotiated(None);
                self.mark(Over::Failed(format!("unusable format ({error:?})")));
                None
            }
        }
    }

    fn on_process(&mut self, stream: &pw::stream::Stream) {
        let open = self.gate.is_open();
        if !open {
            // Nothing is read while the gate is closed, and what was read before is forgotten.
            self.latest = None;
            for capture in self.captures.values_mut() {
                capture.mark(Fault::Ended(StreamEndReason::Blocked));
            }
        }
        let negotiated = self.negotiated;
        let reading = open && self.over.is_none() && negotiated.is_some();
        // The newest buffer wins; an older one goes back to the producer when it is replaced, but
        // what it changed still counts.
        let mut newest = None;
        while let Some(buffer) = stream.dequeue_buffer() {
            if reading && let Some(negotiated) = negotiated {
                let damage = buffer_damage(&buffer, negotiated.size);
                for capture in self.captures.values_mut() {
                    capture.apply_damage(&damage);
                }
            }
            newest = Some(buffer);
        }
        if let (true, Some(mut buffer), Some(negotiated)) = (reading, newest, negotiated) {
            self.consume(&mut buffer, negotiated);
        }
    }

    /// Convert the newest buffer once and hand the image to every capture.
    fn consume(&mut self, buffer: &mut pw::buffer::Buffer<'_>, negotiated: Negotiated) {
        let header = buffer
            .find_meta::<MetaHeader>()
            .map(|header| (header.pts(), header.flags()));
        if header.is_some_and(|(_, flags)| flags.contains(MetaHeaderFlags::CORRUPTED)) {
            return;
        }
        let full = match read_image(buffer, negotiated) {
            Ok(full) => full,
            Err(BufferFault::Empty) => return,
            Err(BufferFault::Bad(why)) => {
                for capture in self.captures.values_mut() {
                    capture.bad_buffer(why);
                }
                return;
            }
        };
        let at = frame_time(header.map(|(pts, _)| pts), mono_now());
        self.latest = Some(Latest {
            size: negotiated.size,
            pixels: Arc::clone(&full),
        });
        let now = Instant::now();
        for capture in self.captures.values_mut() {
            capture.accept(Arc::clone(&full), negotiated.size, at, now);
        }
    }
}

/// Run `f` on the feed's state from a PipeWire callback: skipped if the state is busy, and a panic
/// is caught (it must not unwind into C) and fails the feed.
fn with_feed<R>(state: &Rc<RefCell<FeedState>>, f: impl FnOnce(&mut FeedState) -> R) -> Option<R> {
    let mut state = state.try_borrow_mut().ok()?;
    match catch_unwind(AssertUnwindSafe(|| f(&mut state))) {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::error!("a virtual screen stream callback panicked");
            state.mark(Over::Failed("a stream callback panicked".to_owned()));
            None
        }
    }
}

// ---- the loop ----------------------------------------------------------------------------------

/// The feed on the loop. The listener goes before the stream.
struct Feed {
    _listener: pw::stream::StreamListener<Rc<RefCell<FeedState>>>,
    stream: pw::stream::StreamRc,
    state: Rc<RefCell<FeedState>>,
    node_id: u32,
}

/// The connection to the portal's remote. Listeners before what they listen to.
struct Link {
    /// Node ids the registry reported removed.
    removed: Rc<RefCell<Vec<u32>>>,
    /// The core reported its connection broken.
    failed: Rc<Cell<bool>>,
    _registry_listener: Option<pw::registry::Listener>,
    _registry: Option<pw::registry::RegistryRc>,
    _core_listener: pw::core::Listener,
    core: pw::core::CoreRc,
}

/// Everything the thread owns besides the main loop.
struct Engine {
    /// Before `link`: the stream goes before the core.
    feed: Option<Feed>,
    link: Option<Link>,
    context: pw::context::ContextRc,
    gate: Arc<IoGate>,
    shared: Arc<Shared>,
    /// `Detach` was handled: nothing starts any more.
    detached: bool,
}

impl Engine {
    /// How long the loop may sleep: until the gate is next polled or a frame is due.
    fn wait(&self, now: Instant) -> Duration {
        let Some(feed) = self.feed.as_ref() else {
            // Nothing to watch: only a command (or shutdown) needs the loop's attention.
            return IDLE_TICK;
        };
        let mut wait = TICK;
        if let Ok(state) = feed.state.try_borrow() {
            for capture in state.captures.values() {
                if let Some(due) = capture.wake_at(now) {
                    wait = wait.min(due.saturating_duration_since(now));
                }
            }
        }
        wait.max(MIN_WAIT)
    }

    /// Handle commands and the state callbacks left behind. `false`: every sender is gone.
    fn pump(&mut self, receiver: &Receiver<Command>) -> bool {
        for _ in 0..COMMANDS_PER_TICK {
            match receiver.try_recv() {
                Ok(command) => self.command(command),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
        self.check(Instant::now());
        true
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Attach { fd } => self.attach(fd),
            Command::Open { node_id, size } => {
                if let Err(error) = self.open_feed(node_id, size) {
                    tracing::warn!(%error, "cannot open the virtual screen stream");
                    self.lose(error);
                }
            }
            Command::Resize { size, reply } => {
                let _ = reply.send(self.resize(size));
            }
            Command::Start(request) => self.start(*request),
            Command::SetCrop {
                stream,
                crop,
                reply,
            } => {
                let _ = reply.send(self.set_crop(stream, crop));
            }
            Command::Stop { stream, reply } => {
                self.end(stream, StreamEndReason::Requested);
                let _ = reply.send(Ok(()));
            }
            Command::Detach { reason } => self.detach(reason),
        }
    }

    fn attach(&mut self, fd: OwnedFd) {
        if self.detached {
            return;
        }
        // An earlier connection that was never detached ends here.
        self.detach_link(StreamEndReason::TargetGone);
        let core = match self.context.connect_fd_rc(fd, None) {
            Ok(core) => core,
            Err(error) => {
                tracing::warn!(%error, "cannot connect to the portal's PipeWire remote");
                self.shared.pw_failed(backend(format!(
                    "cannot connect to the PipeWire remote: {error}"
                )));
                return;
            }
        };
        let failed = Rc::new(Cell::new(false));
        let core_listener = {
            let failed = Rc::clone(&failed);
            core.add_listener_local()
                .error(move |id, _seq, res, message| {
                    // Errors on the core object itself mean the connection is broken; errors on a
                    // proxy (a node we may not touch) are the stream's business.
                    if id == pw::sys::PW_ID_CORE {
                        tracing::warn!(res, message, "the PipeWire connection broke");
                        failed.set(true);
                    } else {
                        tracing::debug!(id, res, message, "PipeWire object error");
                    }
                })
                .register()
        };
        let removed = Rc::new(RefCell::new(Vec::new()));
        let (registry, registry_listener) = match core.get_registry_rc() {
            Ok(registry) => {
                let removed = Rc::clone(&removed);
                let listener = registry
                    .add_listener_local()
                    .global_remove(move |id| removed.borrow_mut().push(id))
                    .register();
                (Some(registry), Some(listener))
            }
            Err(error) => {
                tracing::debug!(%error, "no PipeWire registry; node removal goes unnoticed");
                (None, None)
            }
        };
        tracing::info!("connected to the portal's PipeWire remote");
        self.link = Some(Link {
            removed,
            failed,
            _registry_listener: registry_listener,
            _registry: registry,
            _core_listener: core_listener,
            core,
        });
    }

    /// Make the feed and connect it to its node. Format negotiation follows asynchronously.
    fn open_feed(&mut self, node_id: u32, size: PixelSize) -> Result<(), PlatformError> {
        if self.detached || self.feed.is_some() {
            return Err(backend("the stream is already open or the screen is over"));
        }
        let link = self
            .link
            .as_ref()
            .ok_or_else(|| backend("no PipeWire remote"))?;
        if link.failed.get() {
            return Err(backend("the PipeWire connection was lost"));
        }
        let enum_format = format::enum_format(size).map_err(backend_format)?;
        let mut params = vec![pods::buffers_param().map_err(backend_format)?];
        params.extend(pods::meta_params().map_err(backend_format)?);
        let state = Rc::new(RefCell::new(FeedState {
            wanted: size,
            params,
            negotiated: None,
            captures: HashMap::new(),
            latest: None,
            over: None,
            connected: false,
            gate: Arc::clone(&self.gate),
            shared: Arc::clone(&self.shared),
        }));
        let stream = pw::stream::StreamRc::new(
            link.core.clone(),
            STREAM_NAME,
            pw::properties::properties! {
                *pw::keys::NODE_NAME => STREAM_NAME,
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )
        .map_err(backend)?;
        let listener = stream
            .add_local_listener_with_user_data(Rc::clone(&state))
            .state_changed(|_, data, _old, new| {
                with_feed(data, |feed| feed.on_state(&new));
            })
            .param_changed(|stream, data, id, pod| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let Some(params) = with_feed(data, |feed| feed.on_format(pod)).flatten() else {
                    return;
                };
                // Outside the state's borrow: PipeWire may call back while the params are set.
                if send_params(stream, &params).is_err() {
                    with_feed(data, |feed| {
                        feed.mark(Over::Failed("cannot answer the format".to_owned()));
                    });
                }
            })
            .process(|stream, data| {
                with_feed(data, |feed| feed.on_process(stream));
            })
            .register()
            .map_err(backend)?;
        let pod = Pod::from_bytes(&enum_format).ok_or_else(|| backend("format pod"))?;
        stream
            .connect(
                spa::utils::Direction::Input,
                Some(node_id),
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::DONT_RECONNECT,
                &mut [pod],
            )
            .map_err(backend)?;
        tracing::info!(
            node = node_id,
            width = size.width,
            height = size.height,
            "virtual screen stream opened"
        );
        self.feed = Some(Feed {
            _listener: listener,
            stream,
            state,
            node_id,
        });
        Ok(())
    }

    /// Offer `size` instead of the current one.
    fn resize(&mut self, size: PixelSize) -> Result<(), PlatformError> {
        let feed = self.feed.as_ref().ok_or(PlatformError::NotFound)?;
        let enum_format = format::enum_format(size).map_err(backend_format)?;
        let pod = Pod::from_bytes(&enum_format).ok_or_else(|| backend("format pod"))?;
        // Before the call: PipeWire may answer with the new format while it runs.
        with_feed(&feed.state, |state| state.wanted = size);
        feed.stream.update_params(&mut [pod]).map_err(backend)?;
        tracing::info!(
            width = size.width,
            height = size.height,
            "virtual screen resize requested"
        );
        Ok(())
    }

    fn start(&mut self, request: StartRequest) {
        if request.cancelled.load(Ordering::Acquire) {
            return;
        }
        if let Err(error) = self.add_capture(&request) {
            let _ = request.reply.send(Err(error));
            return;
        }
        if request.reply.send(Ok(())).is_err() {
            // The caller gave up and nobody knows the stream id: drop it quietly.
            if let Some(feed) = self.feed.as_ref() {
                feed.state.borrow_mut().captures.remove(&request.id);
            }
            return;
        }
        // The screen may be static: give the new capture the newest image at once.
        if let Some(feed) = self.feed.as_ref() {
            with_feed(&feed.state, |state| {
                if let Some(latest) = state.latest.clone()
                    && let Some(capture) = state.captures.get_mut(&request.id)
                {
                    capture.accept(latest.pixels, latest.size, mono_now(), Instant::now());
                }
            });
        }
    }

    fn add_capture(&mut self, request: &StartRequest) -> Result<(), PlatformError> {
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        validate_crop(request.crop)?;
        let feed = self.feed.as_ref().ok_or(PlatformError::NotFound)?;
        let mut state = feed
            .state
            .try_borrow_mut()
            .map_err(|_| backend("the stream is busy"))?;
        if state.over.is_some() {
            return Err(PlatformError::NotFound);
        }
        let device_size = state.negotiated.map_or(state.wanted, |n| n.size);
        let capture = Capture::new(
            request.id,
            Arc::clone(&request.sink),
            Arc::clone(&self.gate),
            request.crop,
            device_size,
            request.max_fps,
            Vec::new(),
        );
        state.captures.insert(request.id, capture);
        tracing::info!(
            stream = request.id.0,
            max_fps = request.max_fps,
            "virtual screen capture started"
        );
        Ok(())
    }

    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        validate_crop(crop)?;
        let feed = self.feed.as_ref().ok_or(PlatformError::NotFound)?;
        let mut state = feed
            .state
            .try_borrow_mut()
            .map_err(|_| backend("the stream is busy"))?;
        let capture = state
            .captures
            .get_mut(&stream)
            .ok_or(PlatformError::NotFound)?;
        capture.set_crop(crop, Instant::now());
        Ok(())
    }

    /// End one capture: remove it, then tell its sink.
    fn end(&mut self, id: StreamId, reason: StreamEndReason) {
        let Some(feed) = self.feed.as_ref() else {
            return;
        };
        let removed = feed.state.borrow_mut().captures.remove(&id);
        let Some(capture) = removed else {
            return;
        };
        let sink = Arc::clone(capture.sink());
        drop(capture);
        // A capture that dies while the gate is closed died of the gate, whatever else went wrong.
        let reason = if reason != StreamEndReason::Requested && !self.gate.is_open() {
            StreamEndReason::Blocked
        } else {
            reason
        };
        emit(&sink, FrameEvent::Ended { stream: id, reason });
    }

    fn end_all(&mut self, reason: StreamEndReason) {
        let ids: Vec<StreamId> = match self.feed.as_ref() {
            Some(feed) => feed.state.borrow().captures.keys().copied().collect(),
            None => return,
        };
        for id in ids {
            self.end(id, reason);
        }
    }

    /// Drop the feed and the connection after ending the captures with `reason`.
    fn detach_link(&mut self, reason: StreamEndReason) {
        self.end_all(reason);
        // The stream goes before the core.
        self.feed = None;
        self.link = None;
    }

    fn detach(&mut self, reason: StreamEndReason) {
        self.detach_link(reason);
        self.detached = true;
        tracing::info!("disconnected from the PipeWire remote");
    }

    /// The screen cannot go on: tell the worker, end the captures, drop everything.
    fn lose(&mut self, error: PlatformError) {
        self.shared.pw_failed(error);
        self.detach_link(StreamEndReason::TargetGone);
    }

    /// Look at what the callbacks left behind and give due frames out.
    fn check(&mut self, now: Instant) {
        if let Some(link) = self.link.as_ref()
            && link.failed.get()
        {
            self.lose(backend("the PipeWire connection broke"));
            return;
        }
        if !self.gate.is_open() {
            // The captures end; the screen stays and nothing is read until the gate opens.
            if let Some(feed) = self.feed.as_ref() {
                feed.state.borrow_mut().latest = None;
            }
            self.end_all(StreamEndReason::Blocked);
        }
        if let (Some(link), Some(feed)) = (self.link.as_ref(), self.feed.as_ref()) {
            let removed: Vec<u32> = link.removed.borrow_mut().drain(..).collect();
            if removed.contains(&feed.node_id) {
                with_feed(&feed.state, |state| state.mark(Over::Gone));
            }
        }
        let Some(feed) = self.feed.as_ref() else {
            return;
        };
        let (ending, over) = {
            let Ok(mut state) = feed.state.try_borrow_mut() else {
                return;
            };
            let mut ending = Vec::new();
            for (&id, capture) in &mut state.captures {
                capture.deliver_due(now);
                if let Some(Fault::Ended(reason)) = capture.take_fault() {
                    ending.push((id, reason));
                }
            }
            (ending, state.over.take())
        };
        for (id, reason) in ending {
            self.end(id, reason);
        }
        if let Some(over) = over {
            let why = match over {
                Over::Failed(message) => format!("the stream failed ({message})"),
                Over::Gone => "the stream or its node went away".to_owned(),
            };
            tracing::warn!(%why, "the virtual screen is lost");
            self.lose(backend(why));
        }
    }
}

// ---- the thread --------------------------------------------------------------------------------

/// The PipeWire thread. Dropping it stops the thread (bounded) and ends any capture still running.
pub(super) struct StreamThread {
    shutdown: Arc<AtomicBool>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for StreamThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamThread").finish_non_exhaustive()
    }
}

/// Signals that the thread is over, even when it panicked.
struct DoneGuard(SyncSender<()>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

impl StreamThread {
    /// Start the thread and wait (bounded) for its main loop. Work comes with commands.
    pub(super) fn spawn(
        receiver: Receiver<Command>,
        gate: Arc<IoGate>,
        shared: Arc<Shared>,
    ) -> Result<StreamThread, PlatformError> {
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (done_tx, done) = mpsc::sync_channel(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let shutdown = Arc::clone(&shutdown);
            thread::Builder::new()
                .name("crosspane-pipewire-virtual".to_owned())
                .spawn(move || {
                    let _done = DoneGuard(done_tx);
                    run(&receiver, &shutdown, gate, shared, &ready_tx);
                })
                .map_err(backend)?
        };
        let mut this = StreamThread {
            shutdown,
            done,
            thread: Some(thread),
        };
        match ready.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => Ok(this),
            Ok(Err(error)) => {
                this.stop();
                Err(error)
            }
            Err(_) => {
                this.stop();
                Err(PlatformError::Timeout)
            }
        }
    }

    /// Stop the thread and wait for it, bounded. Idempotent.
    pub(super) fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return;
        };
        if self.done.recv_timeout(STOP_TIMEOUT).is_ok() {
            if thread.join().is_err() {
                tracing::warn!("the virtual screen PipeWire thread panicked");
            }
        } else {
            tracing::warn!("the virtual screen PipeWire thread did not stop in time; detaching it");
        }
    }
}

impl Drop for StreamThread {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    receiver: &Receiver<Command>,
    shutdown: &AtomicBool,
    gate: Arc<IoGate>,
    shared: Arc<Shared>,
    ready: &SyncSender<Result<(), PlatformError>>,
) {
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(mainloop) => mainloop,
        Err(error) => {
            let _ = ready.send(Err(backend(error)));
            return;
        }
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(context) => context,
        Err(error) => {
            let _ = ready.send(Err(backend(error)));
            return;
        }
    };
    let mut engine = Engine {
        feed: None,
        link: None,
        context,
        gate,
        shared,
        detached: false,
    };
    let _ = ready.send(Ok(()));
    while !shutdown.load(Ordering::Acquire) {
        let wait = engine.wait(Instant::now());
        mainloop.loop_().iterate(pw::loop_::Timeout::Finite(wait));
        if !engine.pump(receiver) {
            break;
        }
    }
    engine.detach_link(StreamEndReason::Requested);
}
