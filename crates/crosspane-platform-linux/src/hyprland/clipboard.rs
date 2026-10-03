//! Hyprland clipboard metadata and lazy, bounded reads. Native objects stay on one event thread;
//! neither watching nor `kinds` receives content. Promises fetch content only for local pastes.
//!
//! \[E\] Hyprland 0.56, `src/managers/SeatManager.cpp:635–659`, notifies only wl_data_device
//! clients when clearing a selection. ext-data-control metadata can therefore stay stale after
//! a clear; `read` still requests the current source and never returns cached content.
//! Withdrawal destroys only our source: \[E\] `SeatManager.cpp:641–652` resets its current-source
//! destroy listener on replacement. Unlike set_selection(NULL), this preserves a newer selection.

#[path = "clipboard/paste.rs"]
pub(crate) mod paste;
#[path = "clipboard/read.rs"]
pub(crate) mod read;
#[path = "clipboard/wayland.rs"]
mod wayland;

#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use wayland::connection_guard;

use std::fmt;
use std::io::{PipeWriter, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crosspane_platform::{
    ClipKinds, ClipboardEvent, ClipboardHost, EventSink, IoGate, LocalPasteId, PlatformError,
};
use crosspane_types::ClipKind;

const BUDGET: Duration = Duration::from_millis(1500);
const TICK: Duration = Duration::from_millis(10);
type Reply = SyncSender<Result<ClipKinds, PlatformError>>;

enum Action {
    Subscribe(Arc<dyn EventSink<ClipboardEvent>>),
    Kinds,
    Read(ClipKind, PipeWriter, u64),
    Promise(u64, ClipKinds, u64),
    Withdraw(u64),
}

struct Command {
    action: Action,
    deadline: Instant,
    reply: Reply,
}

fn backend(message: &'static str) -> PlatformError {
    PlatformError::Backend(message.into())
}

/// A clipboard handle connected exclusively to the supplied Wayland display (name or socket path).
/// Its `Debug` output contains no selection types, content or private marker.
pub struct HyprlandClipboard {
    commands: SyncSender<Command>,
    gate: Arc<IoGate>,
    pastes: Arc<paste::Pastes>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    #[cfg(test)]
    // The integration test reads this; the library-test build does not.
    #[allow(dead_code)]
    pub(crate) marker: String,
}

impl fmt::Debug for HyprlandClipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyprlandClipboard").finish_non_exhaustive()
    }
}

impl HyprlandClipboard {
    /// Bind the first seat and ext-data-control-v1 on a separate, bounded event thread.
    pub fn new(gate: Arc<IoGate>, display: impl AsRef<Path>) -> Result<Self, PlatformError> {
        let mut random = [0; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(|_| backend("could not create clipboard marker"))?;
        let marker = format!(
            "application/x-crosspane-promise-{:032x}",
            u128::from_ne_bytes(random)
        );
        let (commands, receiver) = mpsc::sync_channel(32);
        let (ready, result) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now() + BUDGET;
        let (thread_stop, path) = (stop.clone(), display.as_ref().to_owned());
        let thread_marker = marker.clone();
        let pastes = paste::Pastes::new(gate.clone());
        let thread_pastes = pastes.clone();
        let thread = std::thread::Builder::new()
            .name("crosspane-clipboard".into())
            .spawn(move || {
                wayland::run(
                    path,
                    thread_stop,
                    thread_marker,
                    thread_pastes,
                    receiver,
                    ready,
                    deadline,
                )
            })
            .map_err(|_| backend("could not start clipboard thread"))?;
        let handle = Self {
            commands,
            gate,
            pastes,
            stop,
            thread: Some(thread),
            #[cfg(test)]
            marker,
        };
        receive(&result, deadline)??;
        Ok(handle)
    }

    fn call(&self, action: Action, deadline: Instant) -> Result<ClipKinds, PlatformError> {
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .try_send(Command {
                action,
                deadline,
                reply,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => PlatformError::Timeout,
                mpsc::TrySendError::Disconnected(_) => backend("clipboard connection ended"),
            })?;
        receive(&result, deadline)?
    }
}

fn receive<T>(result: &Receiver<T>, deadline: Instant) -> Result<T, PlatformError> {
    result
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
            mpsc::RecvTimeoutError::Disconnected => backend("clipboard connection ended"),
        })
}

impl ClipboardHost for HyprlandClipboard {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<ClipboardEvent>>) -> Result<(), PlatformError> {
        self.call(Action::Subscribe(sink), Instant::now() + BUDGET)
            .map(|_| ())
    }

    fn kinds(&self) -> Result<ClipKinds, PlatformError> {
        self.call(Action::Kinds, Instant::now() + BUDGET)
    }

    /// Every zero-byte read, including genuine empty text, is `NotFound`: its EOF cannot
    /// distinguish an empty payload from the stale offer of a cleared source noted above.
    fn read(&mut self, kind: ClipKind, max_bytes: usize) -> Result<Vec<u8>, PlatformError> {
        let deadline = Instant::now() + BUDGET;
        let epoch = self.gate.epoch();
        read::check(&self.gate, epoch, deadline)?;
        let (reader, writer) =
            std::io::pipe().map_err(|_| backend("could not create clipboard pipe"))?;
        read::nonblocking(&reader)?;
        self.call(Action::Read(kind, writer, epoch), deadline)?;
        let data = read::receive(reader, kind, max_bytes, deadline, &self.gate, epoch)?;
        if data.is_empty() {
            return Err(PlatformError::NotFound);
        }
        Ok(data)
    }

    fn promise(&mut self, offer: u64, kinds: ClipKinds) -> Result<(), PlatformError> {
        let epoch = self.gate.epoch();
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if !kinds.text && !kinds.image {
            return Err(backend("empty clipboard promise"));
        }
        self.call(
            Action::Promise(offer, kinds, epoch),
            Instant::now() + BUDGET,
        )
        .map(|_| ())
    }

    fn fulfil(&mut self, paste: LocalPasteId, data: Option<Vec<u8>>) {
        self.pastes.fulfil(paste, data);
    }

    fn withdraw(&mut self, offer: u64) -> Result<(), PlatformError> {
        self.call(Action::Withdraw(offer), Instant::now() + BUDGET)
            .map(|_| ())
    }
}

impl Drop for HyprlandClipboard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.pastes.close();
    }
}
