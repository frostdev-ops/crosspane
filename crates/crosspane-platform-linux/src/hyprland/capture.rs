//! Native objects live on a dedicated Wayland thread. Abort wakes a separate shutdown/delivery
//! thread holding an independent duplicate of the Wayland socket. It shuts down that connection
//! without touching proxies, dispatch, or Wayland backend mutexes, even if dispatch is stuck.
//! Subscription delivery is serialised and generation-fenced there, including `Aborted`.
//! The worker reconnects and rebuilds portals after abort, with bounded backoff. No unsafe code is
//! needed. Every socket owner shuts down the underlying connection on failure or unwinding:
//! shutdown affects every duplicated descriptor, unlike merely dropping a Connection.
mod events;
mod wayland;

use crosspane_platform::{
    CaptureAbort, CaptureEvent, CaptureId, CapturePortal, CaptureStart, EndReason, EventSink,
    InputCapture, IoGate, PlatformError, PortalId,
};
use crosspane_types::{geom::PointDevice, id::DisplayId, input::LockKeys};
use std::{
    fmt,
    net::Shutdown,
    os::unix::net::{UnixDatagram, UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
const CALL_BUDGET: Duration = Duration::from_millis(40);
const POLL: Duration = Duration::from_millis(10);

struct Socket {
    stream: UnixStream,
    lost: AtomicBool,
}
impl Socket {
    fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
    fn lost(&self) {
        self.lost.store(true, Ordering::Release);
        self.shutdown();
    }
}
// Used by both native dispatch and delivery, including constructor failures and sink panics.
pub(super) struct SocketGuard(Arc<Socket>);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}
struct WorkerGuard(Arc<Abort>);
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Command handle for Hyprland's layer-shell capture adapter.
pub struct HyprlandCapture {
    commands: mpsc::Sender<Command>,
    abort: Arc<Abort>,
}
impl fmt::Debug for HyprlandCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyprlandCapture").finish_non_exhaustive()
    }
}
#[derive(Debug)]
struct Abort {
    epoch: AtomicU64,
    next_capture: AtomicU64,
    acknowledged: AtomicU64,
    next_barrier: AtomicU64,
    barrier_reached: AtomicU64,
    wake: UnixDatagram,
}
impl CaptureAbort for Abort {
    fn abort(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        // No capture-thread locks or waiting. A full wake socket is harmless: the independent
        // shutdown thread checks the authoritative cancellation epoch at least every 10 ms.
        let _ = self.wake.send(&[1]);
    }
}
impl Abort {
    fn send_packet(
        &self,
        epoch: u64,
        packet: &[u8; events::SIZE],
        deadline: Option<Instant>,
    ) -> Result<(), PlatformError> {
        loop {
            if epoch != self.epoch.load(Ordering::Acquire) {
                return Err(backend("capture cancelled"));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(PlatformError::Timeout);
            }
            match self.wake.send(packet) {
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_micros(100))
                }
                Err(e) => return Err(backend(e)),
            }
        }
    }
}

pub(super) enum Delivery {
    Connection {
        epoch: u64,
        socket: SocketGuard,
        ready: mpsc::Sender<()>,
    },
    Subscribe {
        sink: Arc<dyn EventSink<CaptureEvent>>,
        locks: LockKeys,
        ready: mpsc::Sender<()>,
    },
}
enum Operation {
    Portals(Vec<CapturePortal>),
    Subscribe(Arc<dyn EventSink<CaptureEvent>>),
    Begin(CaptureId, PortalId),
    End(Option<(DisplayId, PointDevice)>),
    InjectWorkerError,
    Stop,
}
enum Reply {
    Done,
    Started(CaptureStart),
}
struct Command {
    operation: Operation,
    deadline: Instant,
    epoch: u64,
    reply: mpsc::Sender<Result<Reply, PlatformError>>,
}
impl HyprlandCapture {
    /// Connect to `$WAYLAND_DISPLAY`; strips are created per portal on `set_portals`.
    pub fn new(gate: Arc<IoGate>) -> Result<Self, PlatformError> {
        let (wake, receive) = UnixDatagram::pair().map_err(backend)?;
        wake.set_nonblocking(true).map_err(backend)?;
        receive.set_nonblocking(true).map_err(backend)?;
        let abort = Arc::new(Abort {
            epoch: AtomicU64::new(0),
            next_capture: AtomicU64::new(0),
            acknowledged: AtomicU64::new(0),
            next_barrier: AtomicU64::new(0),
            barrier_reached: AtomicU64::new(0),
            wake,
        });
        let (delivery, events) = mpsc::channel();
        let control = abort.clone();
        std::thread::Builder::new()
            .name("hypr-capture-shutdown".into())
            .spawn(move || deliver(events, receive, control))
            .map_err(backend)?;
        let (commands, requests) = mpsc::channel();
        let (ready, initialized) = mpsc::channel();
        let control = abort.clone();
        std::thread::Builder::new()
            .name("hypr-capture".into())
            .spawn(move || worker(requests, delivery, gate, control, ready))
            .map_err(backend)?;
        let handle = Self { commands, abort };
        initialized
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| PlatformError::Timeout)??;
        Ok(handle)
    }
    /// Inject a recoverable dispatch failure for the nested-compositor regression test.
    #[doc(hidden)]
    pub fn inject_worker_error_for_test(&self) -> Result<(), PlatformError> {
        if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
            return Err(PlatformError::Unsupported("nested capture test hook"));
        }
        self.call(Operation::InjectWorkerError).map(|_| ())
    }
    fn call(&self, operation: Operation) -> Result<Reply, PlatformError> {
        let deadline = Instant::now() + CALL_BUDGET;
        let (reply, result) = mpsc::channel();
        let epoch = self.abort.epoch.load(Ordering::Acquire);
        self.commands
            .send(Command {
                operation,
                deadline,
                epoch,
                reply,
            })
            .map_err(|_| backend("capture worker unavailable"))?;
        match result.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => {
                if result.is_err() && epoch != self.abort.epoch.load(Ordering::Acquire) {
                    self.wait_shutdown(deadline);
                }
                result
            }
            Err(_) => {
                // Reserve 10 ms of the shared 50 ms bound for independent emergency rollback.
                self.abort.abort();
                self.wait_shutdown(deadline);
                Err(PlatformError::Timeout)
            }
        }
    }
    fn wait_shutdown(&self, deadline: Instant) {
        let epoch = self.abort.epoch.load(Ordering::Acquire);
        let rollback_deadline = deadline + Duration::from_millis(9);
        while self.abort.acknowledged.load(Ordering::Acquire) < epoch
            && Instant::now() < rollback_deadline
        {
            std::thread::sleep(Duration::from_micros(100));
        }
    }
}
impl InputCapture for HyprlandCapture {
    /// Replacing portals during capture ends that capture with `EndReason::Lost`.
    fn set_portals(&mut self, portals: &[CapturePortal]) -> Result<(), PlatformError> {
        self.call(Operation::Portals(portals.to_vec())).map(|_| ())
    }
    fn subscribe(&mut self, sink: Arc<dyn EventSink<CaptureEvent>>) -> Result<(), PlatformError> {
        self.call(Operation::Subscribe(sink)).map(|_| ())
    }
    fn begin(&mut self, id: CaptureId, portal: PortalId) -> Result<CaptureStart, PlatformError> {
        match self.call(Operation::Begin(id, portal))? {
            Reply::Started(start) => Ok(start),
            Reply::Done => Err(backend("invalid capture response")),
        }
    }
    fn end(&mut self, warp: Option<(DisplayId, PointDevice)>) -> Result<(), PlatformError> {
        self.call(Operation::End(warp)).map(|_| ())
    }
    fn abort_handle(&self) -> Arc<dyn CaptureAbort> {
        self.abort.clone()
    }
    fn set_monitor_local_activity(&mut self, _: bool) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(
            "Wayland local activity monitoring",
        ))
    }
}
impl Drop for HyprlandCapture {
    fn drop(&mut self) {
        self.abort.abort();
        let (reply, _) = mpsc::channel();
        let _ = self.commands.send(Command {
            operation: Operation::Stop,
            deadline: Instant::now(),
            epoch: self.abort.epoch.load(Ordering::Acquire),
            reply,
        });
        // Never join the potentially stuck Wayland thread; independent shutdown releases input.
    }
}
fn worker(
    commands: mpsc::Receiver<Command>,
    delivery: mpsc::Sender<Delivery>,
    gate: Arc<IoGate>,
    abort: Arc<Abort>,
    ready: mpsc::Sender<Result<(), PlatformError>>,
) {
    let _unwind = WorkerGuard(abort.clone());
    let initialized = wayland::Client::new(
        gate.clone(),
        abort.clone(),
        delivery.clone(),
        Instant::now() + Duration::from_millis(1900),
        None,
    );
    let mut client = match initialized {
        Ok(client) => {
            let _ = ready.send(Ok(()));
            Some(client)
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut monitors = client
        .as_ref()
        .map(|c| c.monitor_cache())
        .unwrap_or_default();
    let mut portals = Vec::new();
    let mut subscribed = false;
    let mut backoff = Duration::from_millis(50);
    let mut reconnect_at = Instant::now() + backoff;
    loop {
        if let Some(c) = client.as_mut()
            && c.pump(POLL).is_err()
        {
            c.disconnected();
            client = None;
            backoff = Duration::from_millis(50);
            reconnect_at = Instant::now() + backoff;
        }
        let command = match commands.recv_timeout(if client.is_some() {
            Duration::ZERO
        } else {
            POLL
        }) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if client.is_none() && !portals.is_empty() && Instant::now() >= reconnect_at {
                    let deadline = Instant::now() + CALL_BUDGET;
                    let fresh = wayland::Client::new(
                        gate.clone(),
                        abort.clone(),
                        delivery.clone(),
                        deadline,
                        Some(monitors.clone()),
                    )
                    .and_then(|mut c| {
                        c.set_portals(&portals, deadline)?;
                        Ok(c)
                    });
                    match fresh {
                        Ok(c) => {
                            client = Some(c);
                            backoff = Duration::from_millis(50);
                        }
                        Err(_) => backoff = (backoff * 2).min(Duration::from_secs(1)),
                    }
                    reconnect_at = Instant::now() + backoff;
                } else if let Some(c) = client.as_mut() {
                    // Refresh topology outside the caller's command budget, never while grabbing.
                    c.refresh_monitors();
                    monitors = c.monitor_cache();
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if matches!(command.operation, Operation::Stop) {
            return;
        }
        let result = (|| {
            if Instant::now() >= command.deadline {
                return Err(PlatformError::Timeout);
            }
            if command.epoch != abort.epoch.load(Ordering::Acquire) {
                return Err(backend("capture operation cancelled"));
            }
            if matches!(command.operation, Operation::Begin(..)) && !gate.is_open() {
                return Err(PlatformError::Locked);
            }
            if client.is_none() {
                let mut fresh = wayland::Client::new(
                    gate.clone(),
                    abort.clone(),
                    delivery.clone(),
                    command.deadline,
                    Some(monitors.clone()),
                )?;
                fresh.set_portals(&portals, command.deadline)?;
                client = Some(fresh);
            }
            let c = client.as_mut().ok_or(PlatformError::NotFound)?;
            match command.operation {
                Operation::Portals(new) => {
                    c.set_portals(&new, command.deadline)?;
                    portals = new;
                    Ok(Reply::Done)
                }
                Operation::Subscribe(sink) => {
                    if subscribed {
                        return Err(backend("capture subscribe called twice"));
                    }
                    let (ready, result) = mpsc::channel();
                    delivery
                        .send(Delivery::Subscribe {
                            sink,
                            locks: c.locks(),
                            ready,
                        })
                        .map_err(|_| backend("capture delivery unavailable"))?;
                    let _ = abort.wake.send(&[1]);
                    result
                        .recv_timeout(command.deadline.saturating_duration_since(Instant::now()))
                        .map_err(|_| PlatformError::Timeout)?;
                    subscribed = true;
                    Ok(Reply::Done)
                }
                Operation::Begin(id, portal) => {
                    if !subscribed {
                        return Err(backend("capture is not subscribed"));
                    }
                    c.begin(id, portal, command.deadline).map(Reply::Started)
                }
                Operation::End(warp) => c.end(warp, command.deadline).map(|_| Reply::Done),
                Operation::InjectWorkerError => {
                    c.inject_worker_error();
                    Ok(Reply::Done)
                }
                Operation::Stop => Ok(Reply::Done),
            }
        })();
        let _ = command.reply.send(result);
    }
}
fn deliver(events: mpsc::Receiver<Delivery>, wake: UnixDatagram, abort: Arc<Abort>) {
    let mut socket: Option<(u64, SocketGuard)> = None;
    let mut sink: Option<Arc<dyn EventSink<CaptureEvent>>> = None;
    let mut active: Option<(u64, u64, CaptureId)> = None;
    let mut ended: Option<(u64, u64)> = None;
    loop {
        let epoch = abort.epoch.load(Ordering::Acquire);
        if socket
            .as_ref()
            .is_some_and(|(e, stream)| *e != epoch || stream.0.lost.load(Ordering::Acquire))
        {
            let reason = if socket.as_ref().is_some_and(|(e, _)| *e != epoch) {
                EndReason::Aborted
            } else {
                EndReason::Lost
            };
            if let Some((cancelled, stream)) = socket.take() {
                stream.0.shutdown();
                // Input is free already. Deliver packets committed before the abort wakeup in
                // FIFO order, so discrete events and unaccelerated samples are not discarded.
                // The nonblocking receive stops at the wakeup marker (or at an empty socket if
                // its send buffer was full); it never waits for the capture producer.
                let mut packet = [0; events::SIZE];
                while let Ok(length) = wake.recv(&mut packet) {
                    if length != events::SIZE {
                        break;
                    }
                    match events::decode(&packet) {
                        Some(events::Packet::Event {
                            epoch: e,
                            generation,
                            event,
                        }) if e == cancelled => forward(
                            cancelled,
                            e,
                            generation,
                            event,
                            &sink,
                            &mut active,
                            &mut ended,
                        ),
                        Some(events::Packet::Barrier(token)) => {
                            abort.barrier_reached.fetch_max(token, Ordering::Release);
                        }
                        _ => (),
                    }
                }
            }
            if let Some((e, generation, id)) = active.take() {
                if let Some(sink) = &sink {
                    sink.send(CaptureEvent::Ended { id, reason });
                }
                ended = Some((e, generation));
            }
        }
        abort.acknowledged.store(epoch, Ordering::Release);
        // The control channel is only used before capture: registration and the one subscribe.
        // In particular no event/activation acknowledgement can strand this thread on a channel
        // slot that a stuck capture thread has reserved but not filled.
        let message = if active.is_none() {
            events.try_recv()
        } else {
            Err(mpsc::TryRecvError::Empty)
        };
        let message = match message {
            Ok(message) => Some(message),
            Err(mpsc::TryRecvError::Disconnected) => {
                if let Some((_, stream)) = socket.take() {
                    stream.0.shutdown();
                }
                return;
            }
            Err(mpsc::TryRecvError::Empty) => None,
        };
        if let Some(message) = message {
            match message {
                Delivery::Connection {
                    epoch: e,
                    socket: stream,
                    ready,
                } => {
                    if e != abort.epoch.load(Ordering::Acquire) {
                        stream.0.shutdown();
                    } else {
                        socket = Some((e, stream));
                    }
                    let _ = ready.send(());
                }
                Delivery::Subscribe {
                    sink: new,
                    locks,
                    ready,
                } => {
                    if locks.caps_lock.is_some()
                        || locks.num_lock.is_some()
                        || locks.scroll_lock.is_some()
                    {
                        new.send(CaptureEvent::LockKeys(locks));
                    }
                    new.send(CaptureEvent::KeyboardBlinded(false));
                    sink = Some(new);
                    let _ = ready.send(());
                }
            }
        }
        let mut packet = [0; events::SIZE];
        let length = match wake.recv(&mut packet) {
            Ok(length) => length,
            Err(_) => {
                let mut fd = [rustix::event::PollFd::new(
                    &wake,
                    rustix::event::PollFlags::IN,
                )];
                let timeout = rustix::event::Timespec::try_from(POLL).ok();
                let _ = rustix::event::poll(&mut fd, timeout.as_ref());
                continue;
            }
        };
        if length != events::SIZE {
            continue;
        } // A one-byte packet is just an abort wakeup.
        match events::decode(&packet) {
            Some(events::Packet::Barrier(token)) => {
                abort.barrier_reached.fetch_max(token, Ordering::Release);
            }
            Some(events::Packet::Event {
                epoch: e,
                generation,
                event,
            }) => {
                forward(
                    abort.epoch.load(Ordering::Acquire),
                    e,
                    generation,
                    event,
                    &sink,
                    &mut active,
                    &mut ended,
                );
            }
            None => (),
        }
    }
}
fn forward(
    current: u64,
    e: u64,
    generation: u64,
    event: CaptureEvent,
    sink: &Option<Arc<dyn EventSink<CaptureEvent>>>,
    active: &mut Option<(u64, u64, CaptureId)>,
    ended: &mut Option<(u64, u64)>,
) {
    let Some(sink) = sink else {
        return;
    };
    match event {
        CaptureEvent::Started { id } => {
            if *ended == Some((e, generation)) {
                return;
            }
            sink.send(CaptureEvent::Started { id });
            if e != current {
                sink.send(CaptureEvent::Ended {
                    id,
                    reason: EndReason::Aborted,
                });
                *ended = Some((e, generation));
            } else {
                *active = Some((e, generation, id));
            }
        }
        CaptureEvent::Ended { id, reason } => {
            if *active == Some((e, generation, id)) {
                sink.send(CaptureEvent::Ended { id, reason });
                *active = None;
                *ended = Some((e, generation));
            }
        }
        CaptureEvent::Motion { .. }
        | CaptureEvent::Key { .. }
        | CaptureEvent::Button { .. }
        | CaptureEvent::Scroll { .. } => {
            if active.is_some_and(|(epoch, g, _)| (epoch, g) == (e, generation)) && e == current {
                sink.send(event);
            }
        }
        _ if e == current => sink.send(event),
        _ => (),
    }
}

pub(super) fn backend(error: impl fmt::Display) -> PlatformError {
    PlatformError::Backend(format!("Hyprland capture: {error}"))
}
