//! ext-data-control-v1 is confined to this seam; the handle and pipe reader are protocol-free.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::time::Instant;

use crosspane_platform::{ClipKinds, ClipboardEvent, EventSink, IoGate, PlatformError};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::protocol::{wl_callback, wl_registry, wl_seat};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child,
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
};

use super::{Action, Command, TICK, backend, read};

#[derive(Default)]
struct State {
    globals: Vec<(u32, String, u32)>,
    offers: HashMap<ObjectId, (ExtDataControlOfferV1, Vec<String>)>,
    selection: Option<ObjectId>,
    sink: Option<Arc<dyn EventSink<ClipboardEvent>>>,
    marker: String,
    synced: bool,
    finished: bool,
}

impl State {
    fn selected(&self) -> Option<&(ExtDataControlOfferV1, Vec<String>)> {
        self.selection.as_ref().and_then(|id| self.offers.get(id))
    }

    fn types(&self) -> &[String] {
        self.selected().map_or(&[], |(_, types)| types)
    }

    fn notify(&self) {
        if !read::own(self.types(), &self.marker)
            && let Some(sink) = &self.sink
        {
            sink.send(ClipboardEvent::Changed {
                kinds: read::kinds(self.types()),
            });
        }
    }

    fn remove(&mut self, id: ObjectId) {
        if let Some((offer, _)) = self.offers.remove(&id) {
            offer.destroy();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
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

impl Dispatch<wl_callback::WlCallback, ()> for State {
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

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.offers.insert(id.id(), (id, Vec::new()));
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                if let Some(previous) = state.selection.take() {
                    state.remove(previous);
                }
                state.selection = id.map(|offer| offer.id());
                state.notify();
            }
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                state.remove(offer.id())
            }
            ext_data_control_device_v1::Event::Finished => state.finished = true,
            _ => {}
        }
    }
    event_created_child!(State, ExtDataControlDeviceV1, [0 => (ExtDataControlOfferV1, ())]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some((_, types)) = state.offers.get_mut(&offer.id())
        {
            types.push(mime_type);
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ExtDataControlManagerV1);

struct Source {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    device: ExtDataControlDeviceV1,
    seat: wl_seat::WlSeat,
    manager: ExtDataControlManagerV1,
}

impl Source {
    fn new(path: PathBuf, marker: String, deadline: Instant) -> Result<Self, PlatformError> {
        let connection = connect(path, deadline)?;
        let mut queue = connection.new_event_queue();
        let registry = connection.display().get_registry(&queue.handle(), ());
        let mut state = State {
            marker,
            ..State::default()
        };
        sync(&connection, &mut queue, &mut state, deadline)?;
        let global = |interface| {
            state
                .globals
                .iter()
                .find(|(_, name, version)| name == interface && *version >= 1)
                .map(|(name, _, _)| *name)
        };
        let manager: ExtDataControlManagerV1 = registry.bind(
            global(ExtDataControlManagerV1::interface().name)
                .ok_or(PlatformError::Unsupported("ext-data-control-v1"))?,
            1,
            &queue.handle(),
            (),
        );
        let seat: wl_seat::WlSeat = registry.bind(
            global(wl_seat::WlSeat::interface().name)
                .ok_or(PlatformError::Unsupported("wl_seat"))?,
            1,
            &queue.handle(),
            (),
        );
        let device = manager.get_data_device(&seat, &queue.handle(), ());
        sync(&connection, &mut queue, &mut state, deadline)?;
        Ok(Self {
            connection,
            queue,
            state,
            device,
            seat,
            manager,
        })
    }

    fn command(
        mut self,
        action: Action,
        gate: &IoGate,
        deadline: Instant,
    ) -> (Option<Self>, Result<ClipKinds, PlatformError>) {
        let answer = (|| {
            match action {
                Action::Subscribe(sink) => {
                    self.state.sink = Some(sink);
                    self.state.notify();
                }
                Action::Kinds => {}
                Action::Read(kind, writer, epoch) => {
                    read::check(gate, epoch, deadline)?;
                    if read::own(self.state.types(), &self.state.marker) {
                        return Err(PlatformError::NotFound);
                    }
                    let mime =
                        read::mime(self.state.types(), kind).ok_or(PlatformError::NotFound)?;
                    let offer = self
                        .state
                        .selected()
                        .ok_or(PlatformError::NotFound)?
                        .0
                        .clone();
                    read::check(gate, epoch, deadline)?;
                    // From the first submission attempt, any failure retires the connection.
                    self.state.finished = true;
                    offer
                        .send_request(ext_data_control_offer_v1::Request::Receive {
                            mime_type: mime.into(),
                            fd: writer.as_fd(),
                        })
                        .map_err(|_| backend("clipboard Wayland receive failed"))?;
                    flush_read(gate, epoch, deadline, || self.connection.flush())?;
                    self.state.finished = false;
                }
            }
            Ok(read::kinds(self.state.types()))
        })();
        if self.state.finished {
            (None, answer)
        } else {
            (Some(self), answer)
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // Do not enqueue destructors or flush an abandoned receive; dropping closes its FDs.
        if self.state.finished {
            return;
        }
        self.state.sink = None;
        for (_, (offer, _)) in self.state.offers.drain() {
            offer.destroy();
        }
        self.device.destroy();
        self.manager.destroy();
        if self.seat.version() >= 5 {
            self.seat.release();
        }
        let _ = self.connection.flush();
    }
}

pub(super) fn run(
    path: PathBuf,
    gate: Arc<IoGate>,
    stop: Arc<AtomicBool>,
    marker: String,
    commands: Receiver<Command>,
    ready: SyncSender<Result<(), PlatformError>>,
    deadline: Instant,
) {
    let mut source = match Source::new(path.clone(), marker.clone(), deadline) {
        Ok(source) => {
            let _ = ready.send(Ok(()));
            Some(source)
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut sink = None;
    while !stop.load(Ordering::Acquire) {
        // Pump before each command so queued selection changes cannot be answered from old types.
        if let Some(current) = &mut source {
            if pump(
                &current.connection,
                &mut current.queue,
                &mut current.state,
                Instant::now() + TICK,
            )
            .is_err()
                || current.state.finished
            {
                current.state.finished = true;
                source = None;
            }
        } else {
            std::thread::sleep(TICK);
        }
        match commands.try_recv() {
            Ok(command) => {
                let answer = (|| {
                    if Instant::now() >= command.deadline {
                        return Err(PlatformError::Timeout);
                    }
                    if source.is_none() {
                        let mut next = Source::new(path.clone(), marker.clone(), command.deadline)?;
                        next.state.sink = sink.clone();
                        next.state.notify();
                        source = Some(next);
                    }
                    if let Action::Subscribe(next) = &command.action {
                        sink = Some(next.clone());
                    }
                    let current = source
                        .take()
                        .ok_or_else(|| backend("clipboard connection ended"))?;
                    let (next, answer) = current.command(command.action, &gate, command.deadline);
                    source = next;
                    answer
                })();
                let _ = command.reply.send(answer);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => break,
        }
    }
}

fn flush_read(
    gate: &IoGate,
    epoch: u64,
    deadline: Instant,
    mut flush: impl FnMut() -> Result<(), WaylandError>,
) -> Result<(), PlatformError> {
    loop {
        read::check(gate, epoch, deadline)?;
        match flush() {
            Ok(()) => return read::check(gate, epoch, deadline),
            Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(TICK);
            }
            Err(_) => return Err(backend("clipboard Wayland receive failed")),
        }
    }
}

fn sync(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: Instant,
) -> Result<(), PlatformError> {
    state.synced = false;
    connection.display().sync(&queue.handle(), ());
    while !state.synced {
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        pump(connection, queue, state, deadline)?;
    }
    Ok(())
}

fn pump(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: Instant,
) -> Result<(), PlatformError> {
    queue
        .dispatch_pending(state)
        .map_err(|_| backend("clipboard Wayland dispatch failed"))?;
    let writable = match connection.flush() {
        Ok(()) => false,
        Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => true,
        Err(_) => return Err(backend("clipboard Wayland flush failed")),
    };
    let Some(guard) = queue.prepare_read() else {
        return Ok(());
    };
    let fd = guard.connection_fd();
    let mut fds = [PollFd::new(
        &fd,
        if writable {
            PollFlags::IN | PollFlags::OUT
        } else {
            PollFlags::IN
        },
    )];
    let wait = TICK.min(deadline.saturating_duration_since(Instant::now()));
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: wait.as_nanos() as i64,
    };
    match poll(&mut fds, Some(&timeout)) {
        Ok(_) => {}
        Err(rustix::io::Errno::INTR) => return Ok(()),
        Err(_) => return Err(backend("clipboard Wayland poll failed")),
    }
    if fds[0]
        .revents()
        .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
    {
        match guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => {}
            Err(_) => return Err(backend("clipboard Wayland read failed")),
        }
    }
    queue
        .dispatch_pending(state)
        .map_err(|_| backend("clipboard Wayland dispatch failed"))?;
    Ok(())
}

fn connect(mut path: PathBuf, deadline: Instant) -> Result<Connection, PlatformError> {
    if !path.is_absolute() {
        path = PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR")
                .ok_or_else(|| backend("XDG_RUNTIME_DIR is not set"))?,
        )
        .join(path);
    }
    let address = rustix::net::SocketAddrUnix::new(&path)
        .map_err(|_| backend("invalid Wayland clipboard socket"))?;
    let fd = rustix::net::socket_with(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::NONBLOCK | rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|_| backend("could not create Wayland clipboard socket"))?;
    loop {
        #[cfg(test)]
        connection_guard::check(&path, None)?;
        match rustix::net::connect(&fd, &address) {
            Ok(()) => break,
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
                if Instant::now() >= deadline {
                    return Err(PlatformError::Timeout);
                }
                std::thread::sleep(TICK);
            }
            Err(_) => return Err(backend("could not connect Wayland clipboard socket")),
        }
    }
    #[cfg(test)]
    connection_guard::check(&path, Some(&fd))?;
    Connection::from_socket(UnixStream::from(fd))
        .map_err(|_| backend("could not initialize clipboard connection"))
}

#[cfg(test)]
#[allow(dead_code)]
#[allow(clippy::unwrap_used)]
pub(crate) mod connection_guard {
    // Installed only by the included clipboard integration-test module. Ordinary builds have
    // no hook, and unit tests using owned socket pairs do not install one.
    use super::*;
    use std::collections::HashMap;
    use std::os::fd::OwnedFd;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    type Check = Arc<dyn Fn(Option<u32>) -> Result<(), PlatformError> + Send + Sync>;
    static CHECKS: OnceLock<Mutex<HashMap<PathBuf, Check>>> = OnceLock::new();

    pub(crate) struct Registration(PathBuf);

    pub(crate) fn install(path: PathBuf, check: Check) -> Registration {
        let mut checks = CHECKS.get_or_init(Mutex::default).lock().unwrap();
        assert!(checks.insert(path.clone(), check).is_none());
        Registration(path)
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            CHECKS.get().unwrap().lock().unwrap().remove(&self.0);
        }
    }

    pub(crate) fn check(path: &std::path::Path, fd: Option<&OwnedFd>) -> Result<(), PlatformError> {
        let check = CHECKS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap()
            .get(path)
            .cloned();
        if let Some(check) = check {
            let pid = fd
                .map(|fd| {
                    rustix::net::sockopt::socket_peercred(fd)
                        .map(|peer| peer.pid.as_raw_nonzero().get() as u32)
                        .map_err(|_| backend("could not verify clipboard test peer"))
                })
                .transpose()?;
            check(pid)?;
        }
        Ok(())
    }

    pub(crate) fn connect(path: PathBuf) -> Result<Connection, PlatformError> {
        super::connect(path, Instant::now() + Duration::from_secs(2))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Read;

    // This peer is an unread, owned socket pair, never a compositor. The registry binds seed
    // client-side proxy IDs only; no server, owner display, selection, or clipboard is accessed.
    fn fake_source() -> (Source, UnixStream) {
        let (socket, peer) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue();
        let registry = connection.display().get_registry(&queue.handle(), ());
        let manager: ExtDataControlManagerV1 = registry.bind(1, 1, &queue.handle(), ());
        let seat: wl_seat::WlSeat = registry.bind(2, 1, &queue.handle(), ());
        let device = manager.get_data_device(&seat, &queue.handle(), ());
        let offer: ExtDataControlOfferV1 = registry.bind(3, 1, &queue.handle(), ());
        let mut state = State {
            selection: Some(offer.id()),
            ..State::default()
        };
        state
            .offers
            .insert(offer.id(), (offer, vec!["text/plain".into()]));
        (
            Source {
                connection,
                queue,
                state,
                device,
                seat,
                manager,
            },
            peer,
        )
    }

    fn open_gate() -> Arc<IoGate> {
        let gate = IoGate::new();
        gate.set_engine_permits(true);
        gate.set_session_permits(true);
        gate
    }

    #[test]
    fn receive_submission_descriptor_exhaustion_is_backend() {
        if std::env::var("CROSSPANE_TEST_FD_LIMIT_CHILD").as_deref() != Ok("1") {
            // Lower the descriptor limit only in this owned child, never in the test runner.
            let name = std::thread::current().name().unwrap().to_owned();
            let status = std::process::Command::new("bash")
                .args(["-c", "ulimit -n 128; exec \"$@\"", "c2-fd-limit"])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env("CROSSPANE_TEST_FD_LIMIT_CHILD", "1")
                .status()
                .unwrap();
            assert!(status.success(), "descriptor-exhaustion child failed");
            return;
        }
        let (source, _peer) = fake_source();
        let (mut reader, writer) = std::io::pipe().unwrap();
        read::nonblocking(&reader).unwrap();
        let gate = open_gate();
        let mut descriptors = Vec::new();
        let exhausted = loop {
            match File::open("/dev/null") {
                Ok(file) => {
                    descriptors.push(file);
                    assert!(
                        descriptors.len() < 128,
                        "child descriptor limit was not applied"
                    );
                }
                Err(error) => break error.raw_os_error() == Some(24),
            }
        };
        assert!(exhausted, "expected EMFILE from the owned child limit");
        let (retired, answer) = source.command(
            Action::Read(crosspane_types::ClipKind::Text, writer, gate.epoch()),
            &gate,
            Instant::now() + std::time::Duration::from_millis(30),
        );
        drop(descriptors);
        assert!(
            matches!(answer, Err(PlatformError::Backend(message)) if message == "clipboard Wayland receive failed")
        );
        assert!(retired.is_none());
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn receive_submission_backpressure_expires_and_closes_buffered_writer() {
        let descriptors_before = std::fs::read_dir("/proc/self/fd").unwrap().count();
        for _ in 0..16 {
            let (source, peer) = fake_source();
            let backend = source.connection.backend();
            loop {
                match rustix::net::send(
                    backend.poll_fd(),
                    &[0; 4096],
                    rustix::net::SendFlags::DONTWAIT,
                ) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::AGAIN) => break,
                    Err(error) => panic!("owned socket fill failed: {error}"),
                }
            }
            drop(backend);
            let (mut reader, writer) = std::io::pipe().unwrap();
            read::nonblocking(&reader).unwrap();
            let gate = open_gate();
            let (retired, answer) = source.command(
                Action::Read(crosspane_types::ClipKind::Text, writer, gate.epoch()),
                &gate,
                Instant::now() + std::time::Duration::from_millis(20),
            );
            assert!(matches!(answer, Err(PlatformError::Timeout)));
            assert!(retired.is_none());
            assert_eq!(reader.read(&mut [0]).unwrap(), 0);
            drop((reader, peer));
            assert_eq!(
                std::fs::read_dir("/proc/self/fd").unwrap().count(),
                descriptors_before
            );
        }
    }

    #[test]
    fn receive_flush_checks_gate_before_retry_and_after_success() {
        let gate = open_gate();
        for would_block in [true, false] {
            gate.set_engine_permits(true);
            let mut tries = 0;
            let result = flush_read(
                &gate,
                gate.epoch(),
                Instant::now() + std::time::Duration::from_secs(1),
                || {
                    tries += 1;
                    gate.set_engine_permits(false);
                    if would_block {
                        Err(WaylandError::Io(std::io::ErrorKind::WouldBlock.into()))
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(matches!(result, Err(PlatformError::Locked)));
            assert_eq!(tries, 1);
        }
    }
}
