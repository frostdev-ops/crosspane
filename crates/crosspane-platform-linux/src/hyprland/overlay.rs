//! Click-through layer-shell indicators. Wayland objects and event delivery live on one thread.

use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crosspane_platform::{
    EventSink, Overlay, OverlayAnchor, OverlayEvent, OverlayHost, OverlayId, PlatformError, Rgb8,
};
use crosspane_types::id::DisplayId;
use font8x8::UnicodeFonts;
use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use super::ipc::HyprIpc;

const CALL_TIMEOUT: Duration = Duration::from_millis(45);
const PRESENT_TIMEOUT: Duration = Duration::from_millis(500);
const REFRESH_BACKOFF: Duration = Duration::from_millis(500);
const HEIGHT: u32 = 40;
const TEXT_X: u32 = 16;
const TEXT_Y: u32 = 12;
const ALPHA: u8 = 230;

/// A bounded command handle to the overlay's Wayland event thread.
#[derive(Debug)]
pub struct HyprlandOverlay {
    commands: mpsc::Sender<Command>,
    stop: Arc<AtomicBool>,
    wake: Arc<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

enum Request {
    Subscribe(Arc<dyn EventSink<OverlayEvent>>),
    Show(OverlayId, Overlay),
    Hide(OverlayId),
    #[cfg(test)]
    Panic,
    #[cfg(test)]
    SuppressFrames(OverlayId),
}

struct Command {
    request: Request,
    deadline: Instant,
    reply: mpsc::Sender<Result<(), PlatformError>>,
}

impl HyprlandOverlay {
    pub fn new() -> Result<Self, PlatformError> {
        let (commands, receiver) = mpsc::channel::<Command>();
        let (ready, result) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let wake =
            Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).map_err(backend)?);
        let worker_wake = wake.clone();
        let worker_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("hypr-overlay".into())
            .spawn(move || {
                let mut worker =
                    match catch_unwind(AssertUnwindSafe(|| Worker::new(&worker_stop, worker_wake)))
                    {
                        Ok(Ok(worker)) => worker,
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "overlay worker initialization failed");
                            let _ = ready.send(Err(error));
                            return;
                        }
                        Err(_) => {
                            tracing::warn!("overlay worker panicked during initialization");
                            let _ = ready.send(Err(backend("worker initialization panicked")));
                            return;
                        }
                    };
                if ready.send(Ok(())).is_err() {
                    tracing::warn!("overlay constructor stopped receiving worker initialization");
                    return;
                }
                let outcome =
                    catch_unwind(AssertUnwindSafe(|| worker.run(&receiver, &worker_stop)));
                let cause = match outcome {
                    Ok(Ok(())) if worker_stop.load(Ordering::Acquire) => {
                        backend("overlay host dropped")
                    }
                    Ok(Ok(())) => backend("overlay worker exited unexpectedly"),
                    Ok(Err(error)) => error,
                    Err(_) => backend("overlay worker panicked"),
                };
                // Keep state outside the unwind boundary, so every live ID survives a panic.
                tracing::warn!(%cause, "overlay worker exited");
                worker.state.unavailable_all(&cause);
                let _ = worker.connection.flush();
            })
            .map_err(backend)?;
        match result.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(Self {
                commands,
                stop,
                wake,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                // Do not turn a bounded constructor into a blocking join on a stalled socket.
                stop.store(true, Ordering::Release);
                let _ = signal(&wake);
                Err(PlatformError::Timeout)
            }
        }
    }

    fn call(&self, request: Request) -> Result<(), PlatformError> {
        let deadline = Instant::now() + CALL_TIMEOUT;
        let (reply, result) = mpsc::channel();
        self.commands
            .send(Command {
                request,
                deadline,
                reply,
            })
            .map_err(|_| PlatformError::Backend("overlay connection unavailable".into()))?;
        signal(&self.wake)?;
        result
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => {
                    PlatformError::Backend("overlay connection unavailable".into())
                }
            })?
    }
}

impl OverlayHost for HyprlandOverlay {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<OverlayEvent>>) -> Result<(), PlatformError> {
        self.call(Request::Subscribe(sink))
    }

    fn show(&mut self, id: OverlayId, overlay: &Overlay) -> Result<(), PlatformError> {
        let overlay = Overlay {
            display: overlay.display,
            anchor: overlay.anchor,
            text: truncate_text(&overlay.text),
            accent: overlay.accent,
        };
        self.call(Request::Show(id, overlay))
    }

    fn hide(&mut self, id: OverlayId) -> Result<(), PlatformError> {
        self.call(Request::Hide(id))
    }
}

impl Drop for HyprlandOverlay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = signal(&self.wake);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy)]
struct SurfaceTag {
    id: OverlayId,
    generation: u64,
}

#[derive(Clone, Copy)]
enum Callback {
    Sync,
    Frame { surface: SurfaceTag, revision: u64 },
}

struct Output {
    proxy: wl_output::WlOutput,
    name: String,
    scale: i32,
    pixel_width: u32,
    pixel_height: u32,
    rotated: bool,
    logical_width: Option<u32>,
    xdg: Option<ZxdgOutputV1>,
}

impl Output {
    fn width(&self) -> u32 {
        self.logical_width.unwrap_or_else(|| {
            let pixels = if self.rotated {
                self.pixel_height
            } else {
                self.pixel_width
            };
            pixels.div_ceil(self.scale as u32)
        })
    }
}

struct Surface {
    tag: SurfaceTag,
    overlay: Overlay,
    output: u32,
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    size: (u32, u32),
    buffer: Option<wl_buffer::WlBuffer>,
    configured: bool,
    dirty: bool,
    revision: u64,
    visible: bool,
    presented: bool,
    presentation_deadline: Option<Instant>,
    announce: bool,
    previous: Option<Box<Surface>>,
    #[cfg(test)]
    suppress_frames: bool,
}

impl Surface {
    fn destroy(self) {
        self.layer.destroy();
        self.surface.destroy();
        if let Some(buffer) = self.buffer {
            buffer.destroy();
        }
        if let Some(previous) = self.previous {
            previous.destroy();
        }
    }
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<ZwlrLayerShellV1>,
    output_manager: Option<ZxdgOutputManagerV1>,
    outputs: HashMap<u32, Output>,
    monitor_ids: HashMap<String, DisplayId>,
    surfaces: HashMap<OverlayId, Surface>,
    sink: Option<Arc<dyn EventSink<OverlayEvent>>>,
    generation: u64,
    synced: bool,
    failure: Option<PlatformError>,
    output_generation: u64,
}

impl State {
    fn output_changed(&mut self, id: u32) {
        for surface in self
            .surfaces
            .values_mut()
            .filter(|surface| surface.output == id)
        {
            surface.dirty = true;
            surface.visible = false;
            surface
                .presentation_deadline
                .get_or_insert(Instant::now() + PRESENT_TIMEOUT);
            surface.revision = surface.revision.wrapping_add(1);
        }
    }

    fn emit(&self, event: OverlayEvent) {
        if let Some(sink) = &self.sink {
            // A broken sink must not prevent notification of the remaining live IDs.
            if catch_unwind(AssertUnwindSafe(|| sink.send(event))).is_err() {
                tracing::warn!("overlay event sink panicked");
            }
        }
    }

    fn remove(&mut self, id: OverlayId, unavailable: Option<&PlatformError>) {
        if self.surfaces.contains_key(&id)
            && let Some(cause) = unavailable
        {
            tracing::warn!(overlay_id = id.0, %cause, "overlay unavailable");
            self.emit(OverlayEvent::Unavailable(id));
        }
        if let Some(surface) = self.surfaces.remove(&id) {
            surface.destroy();
        }
    }

    fn unavailable_all(&mut self, cause: &PlatformError) {
        let ids: Vec<_> = self.surfaces.keys().copied().collect();
        // Notify all IDs before destroying anything: even a destructor panic cannot hide a
        // second overlay's loss. Never print a panic payload, which could contain user text.
        for &id in &ids {
            tracing::warn!(overlay_id = id.0, %cause, "overlay unavailable");
            self.emit(OverlayEvent::Unavailable(id));
        }
        for id in ids {
            if catch_unwind(AssertUnwindSafe(|| self.remove(id, None))).is_err() {
                tracing::warn!(
                    overlay_id = id.0,
                    "overlay cleanup panicked after Unavailable"
                );
            }
        }
    }
}

// eventfd carries wakeups only; the channels carry the commands and IPC results.
fn signal(fd: &OwnedFd) -> Result<(), PlatformError> {
    loop {
        match rustix::io::write(fd, &1_u64.to_ne_bytes()) {
            Ok(8) | Err(rustix::io::Errno::AGAIN) => return Ok(()),
            Err(rustix::io::Errno::INTR) => (),
            Ok(_) => return Err(backend("short eventfd write")),
            Err(error) => return Err(backend(error)),
        }
    }
}

fn drain_wakeup(fd: &OwnedFd) -> Result<(), PlatformError> {
    let mut bytes = [0; 8];
    loop {
        match rustix::io::read(fd, &mut bytes) {
            Ok(8) => (),
            Err(rustix::io::Errno::INTR) => (),
            Err(rustix::io::Errno::AGAIN) => return Ok(()),
            Ok(_) => return Err(backend("short eventfd read")),
            Err(error) => return Err(backend(error)),
        }
    }
}

type MonitorReply = (u64, Result<Vec<(String, u32)>, PlatformError>);

struct MonitorRefresh {
    requests: Option<mpsc::Sender<u64>>,
    replies: mpsc::Receiver<MonitorReply>,
    thread: Option<JoinHandle<()>>,
    outstanding: bool,
    next_attempt: Instant,
    backoff: Duration,
}

impl MonitorRefresh {
    fn new(ipc: HyprIpc, wake: Arc<OwnedFd>) -> Result<Self, PlatformError> {
        let (requests, work) = mpsc::channel();
        let (send, replies) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("hypr-overlay-monitors".into())
            .spawn(move || {
                while let Ok(generation) = work.recv() {
                    let result = catch_unwind(AssertUnwindSafe(|| ipc.monitor_ids()))
                        .unwrap_or_else(|_| Err(backend("monitor refresh panicked")));
                    if send.send((generation, result)).is_err() {
                        break;
                    }
                    if let Err(error) = signal(&wake) {
                        tracing::warn!(%error, "could not wake overlay after monitor refresh");
                    }
                }
            })
            .map_err(backend)?;
        Ok(Self {
            requests: Some(requests),
            replies,
            thread: Some(thread),
            outstanding: false,
            next_attempt: Instant::now(),
            backoff: REFRESH_BACKOFF,
        })
    }

    fn needed(&self, state: &State) -> bool {
        state
            .outputs
            .values()
            .any(|output| !output.name.is_empty() && !state.monitor_ids.contains_key(&output.name))
    }

    fn start(&mut self, generation: u64) {
        if let Some(requests) = &self.requests {
            match requests.send(generation) {
                Ok(()) => self.outstanding = true,
                Err(error) => tracing::warn!(%error, "overlay monitor helper unavailable"),
            }
        }
        self.next_attempt = Instant::now() + self.backoff;
    }

    fn update(&mut self, state: &mut State) {
        while let Ok((generation, result)) = self.replies.try_recv() {
            self.outstanding = false;
            match result {
                Ok(monitors) => {
                    // A reply from before a hotplug cannot restore a removed monitor's ID.
                    if generation == state.output_generation {
                        state.monitor_ids = monitors
                            .into_iter()
                            .map(|(name, id)| (name, DisplayId(id)))
                            .collect();
                    }
                    self.backoff = REFRESH_BACKOFF;
                }
                Err(error) => {
                    tracing::warn!(%error, "overlay monitor refresh failed; retaining cached IDs");
                    self.backoff = (self.backoff * 2).min(Duration::from_secs(5));
                }
            }
            self.next_attempt = Instant::now() + self.backoff;
        }
        if !self.outstanding && self.needed(state) && Instant::now() >= self.next_attempt {
            self.start(state.output_generation);
        }
    }

    fn deadline(&self, state: &State) -> Option<Instant> {
        (!self.outstanding && self.needed(state)).then_some(self.next_attempt)
    }
}

impl Drop for MonitorRefresh {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Worker {
    connection: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    wake: Arc<OwnedFd>,
    monitors: MonitorRefresh,
}

impl Worker {
    fn new(stop: &AtomicBool, wake: Arc<OwnedFd>) -> Result<Self, PlatformError> {
        let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| PlatformError::Unsupported("not running under Hyprland"))?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| PlatformError::Backend("XDG_RUNTIME_DIR is not set".into()))?;
        let ipc = HyprIpc::new(&signature, Path::new(&runtime), Duration::from_millis(500));
        let connection = Connection::connect_to_env().map_err(backend)?;
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        connection.display().get_registry(&qh, ());
        let monitors = MonitorRefresh::new(ipc, wake.clone())?;
        let mut worker = Self {
            connection,
            queue,
            qh,
            state: State::default(),
            wake,
            monitors,
        };
        let deadline = Instant::now() + Duration::from_millis(1800);
        // First barrier discovers globals; the second receives output names and logical sizes.
        for _ in 0..2 {
            worker.state.synced = false;
            worker.connection.display().sync(&worker.qh, Callback::Sync);
            while !worker.state.synced {
                if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err(PlatformError::Timeout);
                }
                worker.pump(Some(deadline))?;
            }
        }
        if worker.state.compositor.is_none() {
            return Err(PlatformError::Unsupported("wl_compositor v4 required"));
        }
        if worker.state.shm.is_none() || worker.state.shell.is_none() {
            return Err(PlatformError::Unsupported("shm and layer-shell required"));
        }
        worker.monitors.start(worker.state.output_generation);
        while worker.monitors.outstanding {
            if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            worker.pump(Some(deadline))?;
            worker.monitors.update(&mut worker.state);
        }
        Ok(worker)
    }

    fn run(
        &mut self,
        commands: &mpsc::Receiver<Command>,
        stop: &AtomicBool,
    ) -> Result<(), PlatformError> {
        while !stop.load(Ordering::Acquire) {
            self.queue
                .dispatch_pending(&mut self.state)
                .map_err(backend)?;
            while let Ok(command) = commands.try_recv() {
                let result = if Instant::now() >= command.deadline {
                    Err(PlatformError::Timeout)
                } else {
                    self.command(command.request)
                };
                let _ = command.reply.send(result);
            }
            self.monitors.update(&mut self.state);
            self.expire_presentations();
            self.draw();
            let deadline = self
                .state
                .surfaces
                .values()
                .filter_map(|surface| surface.presentation_deadline)
                .chain(self.monitors.deadline(&self.state))
                .min();
            if !stop.load(Ordering::Acquire) {
                self.pump(deadline)?;
            }
        }
        Ok(())
    }

    fn expire_presentations(&mut self) {
        let ids: Vec<_> = self
            .state
            .surfaces
            .iter()
            .filter_map(|(&id, surface)| {
                surface
                    .presentation_deadline
                    .filter(|deadline| Instant::now() >= *deadline)
                    .map(|_| id)
            })
            .collect();
        for id in ids {
            self.state.remove(
                id,
                Some(&backend(
                    "overlay presentation did not complete within 500 ms",
                )),
            );
        }
    }

    fn pump(&mut self, deadline: Option<Instant>) -> Result<(), PlatformError> {
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(backend)?;
        if let Some(error) = self.state.failure.take() {
            return Err(error);
        }
        let writable = match self.connection.flush() {
            Ok(()) => false,
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == ErrorKind::WouldBlock =>
            {
                true
            }
            Err(error) => return Err(backend(error)),
        };
        if let Some(guard) = self.connection.prepare_read() {
            let flags = PollFlags::IN
                | if writable {
                    PollFlags::OUT
                } else {
                    PollFlags::empty()
                };
            let mut fds = [
                PollFd::new(&self.connection, flags),
                PollFd::new(&self.wake, PollFlags::IN),
            ];
            let timeout = deadline
                .map(|deadline| {
                    Timespec::try_from(deadline.saturating_duration_since(Instant::now()))
                })
                .transpose()
                .map_err(backend)?;
            match poll(&mut fds, timeout.as_ref()) {
                Ok(_) => {
                    if fds[1].revents().contains(PollFlags::IN) {
                        drain_wakeup(&self.wake)?;
                    }
                    if fds[0]
                        .revents()
                        .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
                    {
                        match guard.read() {
                            Ok(_) => (),
                            Err(wayland_client::backend::WaylandError::Io(error))
                                if error.kind() == ErrorKind::WouldBlock => {}
                            Err(error) => return Err(backend(error)),
                        }
                    }
                }
                Err(rustix::io::Errno::INTR) => (),
                Err(error) => return Err(backend(error)),
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(backend)?;
        if let Some(error) = self.state.failure.take() {
            return Err(error);
        }
        Ok(())
    }

    fn command(&mut self, request: Request) -> Result<(), PlatformError> {
        match request {
            Request::Subscribe(sink) => {
                if self.state.sink.is_some() {
                    return Err(PlatformError::Backend("overlay already subscribed".into()));
                }
                self.state.sink = Some(sink);
                for (&id, surface) in &self.state.surfaces {
                    if surface.visible {
                        self.state.emit(OverlayEvent::Visible(id));
                    }
                }
                Ok(())
            }
            Request::Hide(id) => {
                self.state.remove(id, None);
                Ok(())
            }
            Request::Show(id, overlay) => self.show(id, overlay),
            #[cfg(test)]
            Request::Panic => panic!("overlay worker panic fixture"),
            #[cfg(test)]
            Request::SuppressFrames(id) => {
                self.state
                    .surfaces
                    .get_mut(&id)
                    .ok_or(PlatformError::NotFound)?
                    .suppress_frames = true;
                Ok(())
            }
        }
    }

    fn show(&mut self, id: OverlayId, overlay: Overlay) -> Result<(), PlatformError> {
        let (&output_id, output) = self
            .state
            .outputs
            .iter()
            .find(|(_, output)| self.state.monitor_ids.get(&output.name) == Some(&overlay.display))
            .ok_or(PlatformError::NotFound)?;
        let (width, height) = logical_size(&overlay.text, output.width())?;
        if let Some(surface) = self.state.surfaces.get_mut(&id)
            && surface.output == output_id
        {
            if surface.overlay != overlay {
                if surface.size != (width, height) {
                    surface.layer.set_size(width, height);
                    surface.size = (width, height);
                }
                if surface.overlay.anchor != overlay.anchor {
                    surface.layer.set_anchor(anchor(overlay.anchor));
                }
                surface.overlay = overlay;
                surface.dirty = true;
                surface.visible = false;
                surface.announce = true;
                surface
                    .presentation_deadline
                    .get_or_insert(Instant::now() + PRESENT_TIMEOUT);
                // Invalidate a callback for the preceding text before dispatching it.
                surface.revision = surface.revision.wrapping_add(1);
            } else if surface.visible {
                self.state.emit(OverlayEvent::Visible(id));
            } else {
                surface.announce = true;
                surface
                    .presentation_deadline
                    .get_or_insert(Instant::now() + PRESENT_TIMEOUT);
            }
            return Ok(());
        }
        let compositor = self
            .state
            .compositor
            .as_ref()
            .ok_or(PlatformError::NotFound)?;
        let shell = self.state.shell.as_ref().ok_or(PlatformError::NotFound)?;
        self.state.generation = self.state.generation.wrapping_add(1);
        let tag = SurfaceTag {
            id,
            generation: self.state.generation,
        };
        let surface = compositor.create_surface(&self.qh, tag);
        let region = compositor.create_region(&self.qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        surface.set_buffer_scale(output.scale);
        let layer = shell.get_layer_surface(
            &surface,
            Some(&output.proxy),
            Layer::Overlay,
            "crosspane-overlay".into(),
            &self.qh,
            tag,
        );
        layer.set_size(width, height);
        layer.set_anchor(anchor(overlay.anchor));
        layer.set_margin(24, 24, 24, 24);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.commit();
        let old = self.state.surfaces.insert(
            id,
            Surface {
                tag,
                overlay,
                output: output_id,
                surface,
                layer,
                size: (width, height),
                buffer: None,
                configured: false,
                dirty: true,
                revision: 0,
                visible: false,
                presented: false,
                presentation_deadline: Some(Instant::now() + PRESENT_TIMEOUT),
                announce: true,
                previous: None,
                #[cfg(test)]
                suppress_frames: false,
            },
        );
        if let Some(mut old) = old {
            // Rapid moves retain the last presented surface, rather than a pending replacement.
            let previous = if let Some(previous) = old.previous.take() {
                old.destroy();
                Some(previous)
            } else if old.presented {
                Some(Box::new(old))
            } else {
                old.destroy();
                None
            };
            if let Some(surface) = self.state.surfaces.get_mut(&id) {
                surface.previous = previous;
            }
        }
        Ok(())
    }

    fn draw(&mut self) {
        let ids: Vec<_> = self
            .state
            .surfaces
            .iter()
            .filter_map(|(&id, surface)| (surface.configured && surface.dirty).then_some(id))
            .collect();
        for id in ids {
            if let Err(error) = self.draw_one(id) {
                self.state.remove(id, Some(&error));
            }
        }
    }

    fn draw_one(&mut self, id: OverlayId) -> Result<(), PlatformError> {
        let surface = self
            .state
            .surfaces
            .get_mut(&id)
            .ok_or(PlatformError::NotFound)?;
        let output = self
            .state
            .outputs
            .get(&surface.output)
            .ok_or(PlatformError::NotFound)?;
        let scale = u32::try_from(output.scale).map_err(backend)?;
        let raster = rasterise(
            &surface.overlay.text,
            surface.overlay.accent,
            scale,
            output.width(),
        )?;
        let size = (raster.width / scale, HEIGHT);
        if surface.size != size {
            surface.layer.set_size(size.0, size.1);
            surface.size = size;
        }
        let file = File::from(
            rustix::fs::memfd_create("crosspane-overlay", rustix::fs::MemfdFlags::CLOEXEC)
                .map_err(backend)?,
        );
        file.set_len(raster.pixels.len() as u64).map_err(backend)?;
        (&file).write_all(&raster.pixels).map_err(backend)?;
        let shm = self.state.shm.as_ref().ok_or(PlatformError::NotFound)?;
        let pool = shm.create_pool(
            file.as_fd(),
            i32::try_from(raster.pixels.len()).map_err(backend)?,
            &self.qh,
            (),
        );
        let buffer = pool.create_buffer(
            0,
            raster.width as i32,
            raster.height as i32,
            (raster.width * 4) as i32,
            wl_shm::Format::Argb8888,
            &self.qh,
            (),
        );
        pool.destroy();
        surface.surface.set_buffer_scale(output.scale);
        surface.surface.attach(Some(&buffer), 0, 0);
        surface
            .surface
            .damage_buffer(0, 0, raster.width as i32, raster.height as i32);
        surface.revision = surface.revision.wrapping_add(1);
        surface.surface.frame(
            &self.qh,
            Callback::Frame {
                surface: surface.tag,
                revision: surface.revision,
            },
        );
        surface.surface.commit();
        // Each buffer has its own immutable memfd: destroying the old proxy cannot overwrite a
        // buffer still being read by the compositor, and the server retains its mapping as needed.
        if let Some(old) = surface.buffer.replace(buffer) {
            old.destroy();
        }
        surface.dirty = false;
        surface.visible = false;
        Ok(())
    }
}

fn anchor(value: OverlayAnchor) -> Anchor {
    match value {
        OverlayAnchor::TopCenter => Anchor::Top,
        OverlayAnchor::TopRight => Anchor::Top | Anchor::Right,
        OverlayAnchor::BottomRight => Anchor::Bottom | Anchor::Right,
        OverlayAnchor::Center => Anchor::empty(),
    }
}

fn backend(error: impl std::fmt::Display) -> PlatformError {
    PlatformError::Backend(format!("overlay backend: {error}"))
}

fn truncate_text(text: &str) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(128).collect();
    if chars.next().is_some() {
        result.pop();
        result.push('…');
    }
    result
}

fn logical_size(text: &str, output_width: u32) -> Result<(u32, u32), PlatformError> {
    if output_width == 0 {
        return Err(backend("output logical width is not available"));
    }
    let width = u32::try_from(text.chars().count())
        .ok()
        .and_then(|count| count.checked_mul(16))
        .and_then(|width| width.checked_add(TEXT_X + 12))
        .filter(|&width| width <= i32::MAX as u32)
        .ok_or_else(|| PlatformError::Backend("overlay dimensions out of range".into()))?;
    Ok((width.min(output_width), HEIGHT))
}

struct Raster {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

fn glyph(ch: char) -> [u8; 8] {
    match ch {
        '→' => [0x00, 0x10, 0x30, 0x7f, 0x30, 0x10, 0x00, 0x00],
        '—' => [0x00, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00],
        '…' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x49, 0x00],
        _ => font8x8::BASIC_FONTS
            .get(ch)
            .or_else(|| font8x8::LATIN_FONTS.get(ch))
            .or_else(|| font8x8::GREEK_FONTS.get(ch))
            .or_else(|| font8x8::HIRAGANA_FONTS.get(ch))
            .unwrap_or([0x3c, 0x66, 0x60, 0x30, 0x18, 0, 0x18, 0]),
    }
}

fn rasterise(
    text: &str,
    accent: Rgb8,
    scale: u32,
    output_width: u32,
) -> Result<Raster, PlatformError> {
    let text = truncate_text(text);
    let (logical_width, logical_height) = logical_size(&text, output_width)?;
    let dimensions = || PlatformError::Backend("overlay buffer dimensions out of range".into());
    if scale == 0 {
        return Err(dimensions());
    }
    let width = logical_width.checked_mul(scale).ok_or_else(dimensions)?;
    let height = logical_height.checked_mul(scale).ok_or_else(dimensions)?;
    let len = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .filter(|&n| n <= i32::MAX as u32)
        .ok_or_else(dimensions)?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(len as usize).map_err(backend)?;
    pixels.resize(len as usize, 0);
    let glyphs: Vec<_> = text.chars().map(glyph).collect();
    let radius = i64::from((8 * scale).min(width / 2));
    for y in 0..height {
        for x in 0..width {
            let dx =
                (i64::from(x) * 2 + 1 - i64::from(width)).abs() - (i64::from(width) - 2 * radius);
            let dy =
                (i64::from(y) * 2 + 1 - i64::from(height)).abs() - (i64::from(height) - 2 * radius);
            if dx > 0 && dy > 0 && dx * dx + dy * dy > 4 * radius * radius {
                continue;
            }
            let lx = x / scale;
            let ly = y / scale;
            let text_pixel = if lx >= TEXT_X && (TEXT_Y..TEXT_Y + 16).contains(&ly) {
                let column = (lx - TEXT_X) / 16;
                glyphs.get(column as usize).is_some_and(|glyph| {
                    glyph[((ly - TEXT_Y) / 2) as usize] & (1 << (((lx - TEXT_X) % 16) / 2)) != 0
                })
            } else {
                false
            };
            let color = if text_pixel {
                Rgb8 {
                    r: 255,
                    g: 255,
                    b: 255,
                }
            } else if lx < 4 {
                accent
            } else {
                Rgb8 {
                    r: 31,
                    g: 41,
                    b: 55,
                }
            };
            let premultiply = |channel: u8| (u32::from(channel) * u32::from(ALPHA) + 127) / 255;
            let argb = (u32::from(ALPHA) << 24)
                | (premultiply(color.r) << 16)
                | (premultiply(color.g) << 8)
                | premultiply(color.b);
            let offset = ((y * width + x) * 4) as usize;
            pixels[offset..offset + 4].copy_from_slice(&argb.to_le_bytes());
        }
    }
    Ok(Raster {
        width,
        height,
        pixels,
    })
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" if version >= 4 => {
                    state.compositor = Some(registry.bind(name, version.min(6), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => {
                    state.shell = Some(registry.bind(name, version.min(5), qh, ()))
                }
                "zxdg_output_manager_v1" => {
                    let manager: ZxdgOutputManagerV1 = registry.bind(name, version.min(3), qh, ());
                    for (&id, output) in &mut state.outputs {
                        output.xdg = Some(manager.get_xdg_output(&output.proxy, qh, id));
                    }
                    state.output_manager = Some(manager);
                }
                "wl_output" if version >= 4 => {
                    let proxy = registry.bind(name, 4, qh, name);
                    let xdg = state
                        .output_manager
                        .as_ref()
                        .map(|manager| manager.get_xdg_output(&proxy, qh, name));
                    state.outputs.insert(
                        name,
                        Output {
                            proxy,
                            name: String::new(),
                            scale: 1,
                            pixel_width: 0,
                            pixel_height: 0,
                            rotated: false,
                            logical_width: None,
                            xdg,
                        },
                    );
                    state.output_generation = state.output_generation.wrapping_add(1);
                }
                "wl_output" => {
                    state.failure = Some(PlatformError::Unsupported("wl_output v4 name required"))
                }
                _ => (),
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state.outputs.remove(&name) {
                    state.output_generation = state.output_generation.wrapping_add(1);
                    state.monitor_ids.remove(&output.name);
                    let ids: Vec<_> = state
                        .surfaces
                        .iter()
                        .filter_map(|(&id, surface)| {
                            (surface.output == name
                                || surface
                                    .previous
                                    .as_ref()
                                    .is_some_and(|old| old.output == name))
                            .then_some(id)
                        })
                        .collect();
                    for id in ids {
                        state.remove(id, Some(&backend("overlay output removed")));
                    }
                    if let Some(xdg) = output.xdg {
                        xdg.destroy();
                    }
                    output.proxy.release();
                }
            }
            _ => (),
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => output.name = name,
            wl_output::Event::Scale { factor } if factor > 0 => {
                output.scale = factor;
                state.output_changed(*id);
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) && width > 0 && height > 0 => {
                output.pixel_width = width as u32;
                output.pixel_height = height as u32;
                state.output_changed(*id);
            }
            wl_output::Event::Geometry {
                transform: WEnum::Value(transform),
                ..
            } => {
                output.rotated = matches!(
                    transform,
                    wl_output::Transform::_90
                        | wl_output::Transform::_270
                        | wl_output::Transform::Flipped90
                        | wl_output::Transform::Flipped270
                );
                state.output_changed(*id);
            }
            _ => (),
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_output_v1::Event::LogicalSize { width, .. } = event
            && width > 0
            && let Some(output) = state.outputs.get_mut(id)
        {
            output.logical_width = Some(width as u32);
            state.output_changed(*id);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, SurfaceTag> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        tag: &SurfaceTag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, zwlr_layer_surface_v1::Event::Closed) {
            if state.surfaces.get(&tag.id).is_some_and(|surface| {
                surface.tag.generation == tag.generation
                    || surface
                        .previous
                        .as_ref()
                        .is_some_and(|old| old.tag.generation == tag.generation)
            }) {
                state.remove(tag.id, Some(&backend("layer surface closed")));
            }
            return;
        }
        let Some(surface) = state
            .surfaces
            .get_mut(&tag.id)
            .filter(|surface| surface.tag.generation == tag.generation)
        else {
            return;
        };
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            layer.ack_configure(serial);
            surface.configured = true;
            surface.dirty = true;
            surface.visible = false;
            surface
                .presentation_deadline
                .get_or_insert(Instant::now() + PRESENT_TIMEOUT);
            surface.revision = surface.revision.wrapping_add(1);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, Callback> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        callback: &Callback,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !matches!(event, wl_callback::Event::Done { .. }) {
            return;
        }
        match callback {
            Callback::Sync => state.synced = true,
            Callback::Frame {
                surface: tag,
                revision,
            } => {
                if let Some(surface) = state.surfaces.get_mut(&tag.id)
                    && surface.tag.generation == tag.generation
                    && surface.revision == *revision
                    && !surface.dirty
                    && !surface.visible
                {
                    #[cfg(test)]
                    if surface.suppress_frames {
                        return;
                    }
                    surface.visible = true;
                    surface.presented = true;
                    surface.presentation_deadline = None;
                    let announce = std::mem::take(&mut surface.announce);
                    if let Some(previous) = surface.previous.take() {
                        previous.destroy();
                    }
                    if announce {
                        state.emit(OverlayEvent::Visible(tag.id));
                    }
                }
            }
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_region::WlRegion);
impl Dispatch<wl_surface::WlSurface, SurfaceTag> for State {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &SurfaceTag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(State: ignore ZwlrLayerShellV1);
delegate_noop!(State: ignore ZxdgOutputManagerV1);

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ipc_timeout_keeps_cached_ids_and_helper_can_retry() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;
        let root =
            std::env::temp_dir().join(format!("crosspane-overlay-ipc-{}", std::process::id()));
        let dir = root.join("hypr/mock");
        std::fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(dir.join(".socket.sock")).unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 32];
                let n = stream.read(&mut request).unwrap();
                assert_eq!(&request[..n], b"j/monitors");
                if attempt == 0 {
                    // Deliberately miss the helper's 500 ms IPC deadline.
                    std::thread::sleep(Duration::from_millis(600));
                } else {
                    stream
                        .write_all(br#"[{"name":"cached","id":8},{"name":"new","id":9}]"#)
                        .unwrap();
                }
            }
        });
        let wake = Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).unwrap());
        let mut helper = MonitorRefresh::new(
            HyprIpc::new("mock", &root, Duration::from_millis(500)),
            wake.clone(),
        )
        .unwrap();
        let mut state = State::default();
        state.monitor_ids.insert("cached".into(), DisplayId(8));
        helper.start(state.output_generation);
        let start = Instant::now();
        helper.update(&mut state);
        assert!(start.elapsed() < CALL_TIMEOUT, "IPC blocked dispatch");
        let mut fds = [PollFd::new(&wake, PollFlags::IN)];
        let timeout = Timespec::try_from(Duration::from_secs(2)).unwrap();
        assert_eq!(poll(&mut fds, Some(&timeout)).unwrap(), 1);
        drain_wakeup(&wake).unwrap();
        helper.update(&mut state);
        assert!(!helper.outstanding);
        assert_eq!(state.monitor_ids["cached"], DisplayId(8));
        assert!(
            helper
                .next_attempt
                .saturating_duration_since(Instant::now())
                >= REFRESH_BACKOFF
        );
        std::thread::sleep(
            helper
                .next_attempt
                .saturating_duration_since(Instant::now()),
        );
        helper.start(state.output_generation);
        assert_eq!(poll(&mut fds, Some(&timeout)).unwrap(), 1);
        helper.update(&mut state);
        assert_eq!(state.monitor_ids["cached"], DisplayId(8));
        assert_eq!(state.monitor_ids["new"], DisplayId(9));
        drop(helper);
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_frame_callback_reports_unavailable_after_deadline() {
        if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
            eprintln!("skipped: frame timeout test needs a nested Hyprland");
            return;
        }
        let display = DisplayId(
            HyprIpc::from_env()
                .unwrap()
                .monitor_ids()
                .unwrap()
                .remove(0)
                .1,
        );
        let mut host = HyprlandOverlay::new().unwrap();
        let (send, events) = mpsc::channel();
        host.subscribe(Arc::new(move |event| {
            let _ = send.send(event);
        }))
        .unwrap();
        let id = OverlayId(713);
        let mut overlay = Overlay {
            display,
            anchor: OverlayAnchor::BottomRight,
            text: "frame fixture".into(),
            accent: Rgb8 { r: 255, g: 0, b: 0 },
        };
        host.show(id, &overlay).unwrap();
        assert_eq!(
            events.recv_timeout(Duration::from_millis(200)).unwrap(),
            OverlayEvent::Visible(id)
        );
        host.call(Request::SuppressFrames(id)).unwrap();
        overlay.text = "frame feedback lost".into();
        let start = Instant::now();
        host.show(id, &overlay).unwrap();
        assert_eq!(
            events.recv_timeout(Duration::from_millis(650)).unwrap(),
            OverlayEvent::Unavailable(id)
        );
        assert!(start.elapsed() >= PRESENT_TIMEOUT);
    }

    #[test]
    fn text_is_truncated_by_characters_and_width_is_clamped() {
        assert_eq!(truncate_text("HUD"), "HUD");
        assert_eq!(truncate_text(&"é".repeat(128)), "é".repeat(128));
        let text = truncate_text(&"é".repeat(129));
        assert_eq!(text.chars().count(), 128);
        assert_eq!(text, format!("{}…", "é".repeat(127)));
        assert_eq!(logical_size(&text, 4096).unwrap(), (2076, 40));
        assert_eq!(logical_size(&text, 320).unwrap(), (320, 40));
        let raster = rasterise(&"é".repeat(200), Rgb8 { r: 255, g: 0, b: 0 }, 2, 320).unwrap();
        assert_eq!((raster.width, raster.height), (640, 80));
    }

    #[test]
    fn arrow_and_dash_glyphs_are_rendered() {
        assert_eq!(glyph('→'), [0, 0x10, 0x30, 0x7f, 0x30, 0x10, 0, 0]);
        assert_eq!(glyph('—')[3], 0xff);
        assert_eq!(glyph('—')[4], 0xff);
        let raster = rasterise("→ —", Rgb8 { r: 0, g: 0, b: 255 }, 2, 1920).unwrap();
        let pixel = |x: u32, y: u32| {
            let offset = ((y * 2 * raster.width + x * 2) * 4) as usize;
            u32::from_le_bytes(raster.pixels[offset..offset + 4].try_into().unwrap())
        };
        assert_eq!(pixel(TEXT_X, TEXT_Y + 6), 0xe6e6e6e6); // Arrow shaft.
        assert_eq!(pixel(TEXT_X + 12, TEXT_Y + 6), 0xe6e6e6e6); // Arrow tip.
        assert_eq!(pixel(TEXT_X + 8, TEXT_Y + 2), 0xe6e6e6e6); // Top diagonal.
        assert_eq!(pixel(TEXT_X + 12, TEXT_Y + 4), 0xe61c2532); // Outside diagonal.
        assert_eq!(pixel(TEXT_X + 32 + 14, TEXT_Y + 6), 0xe6e6e6e6); // Em dash.
    }

    #[test]
    fn worker_panic_reports_every_live_overlay_unavailable() {
        if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
            eprintln!("skipped: worker panic test needs a nested Hyprland");
            return;
        }
        let display = DisplayId(
            HyprIpc::from_env()
                .unwrap()
                .monitor_ids()
                .unwrap()
                .remove(0)
                .1,
        );
        let mut host = HyprlandOverlay::new().unwrap();
        let (send, events) = mpsc::channel();
        host.subscribe(Arc::new(move |event| {
            let _ = send.send(event);
        }))
        .unwrap();
        let ids = [OverlayId(711), OverlayId(712)];
        for id in ids {
            host.show(
                id,
                &Overlay {
                    display,
                    anchor: OverlayAnchor::BottomRight,
                    text: "panic fixture".into(),
                    accent: Rgb8 { r: 255, g: 0, b: 0 },
                },
            )
            .unwrap();
            assert_eq!(
                events.recv_timeout(Duration::from_millis(200)).unwrap(),
                OverlayEvent::Visible(id)
            );
        }
        assert!(host.call(Request::Panic).is_err());
        let mut lost: Vec<_> = (0..ids.len())
            .map(|_| events.recv_timeout(Duration::from_secs(1)).unwrap())
            .collect();
        lost.sort_by_key(|event| match event {
            OverlayEvent::Visible(id) | OverlayEvent::Unavailable(id) => *id,
        });
        assert_eq!(lost, ids.map(OverlayEvent::Unavailable));
    }

    #[test]
    fn known_glyph_pixels_at_output_scale() {
        let accent = Rgb8 { r: 255, g: 0, b: 0 };
        for scale in [1, 2, 3] {
            let raster = rasterise("A A", accent, scale, 1920).unwrap();
            assert_eq!((raster.width, raster.height), (76 * scale, 40 * scale));
            let pixel = |x: u32, y: u32| {
                let offset = (((y * scale) * raster.width + x * scale) * 4) as usize;
                u32::from_le_bytes(raster.pixels[offset..offset + 4].try_into().unwrap())
            };
            assert_eq!(pixel(0, 0), 0);
            assert_eq!(pixel(1, 20), 0xe6e60000);
            assert_eq!(pixel(8, 20), 0xe61c2532);
            assert_eq!(pixel(16, 12), 0xe61c2532); // A row 0, column 0: off.
            assert_eq!(pixel(20, 12), 0xe6e6e6e6); // A row 0, column 2: on, doubled.
            assert_eq!(pixel(21, 13), 0xe6e6e6e6);
            assert_eq!(pixel(32, 12), 0xe61c2532); // Space.
            assert_eq!(pixel(52, 12), 0xe6e6e6e6); // Second A.
            assert_eq!(pixel(16, 26), 0xe61c2532); // Blank final glyph row.
        }
    }
}
