//! Hyprland virtual input. Native objects and XKB state stay on one Wayland thread; the two
//! command handles share that source and seat. Hyprland supplies repeat, so only transitions are
//! submitted. Device destruction does not release keys by default on 0.56: cleanup sends ups.
//!
//! A failed output or keymap refresh (the watcher could not read the configuration, or it could
//! not be applied) does not end the worker. The worker *pauses*: it releases every held key and
//! button, answers key-downs, button-downs, motion, scroll and lock changes with the retryable
//! [`PlatformError::Timeout`] (key-ups, button-ups and recovery are still honoured), binds no new
//! pointer from the unknown output map, and asks again with backoff (100 ms doubling to 2 s). The
//! first configuration that is read and applied in full, with no output or keyboard
//! configuration change seen since the read began, resumes it. A closed `IoGate` is independent:
//! both must be clear to inject. A lost Wayland connection, a lost configuration watcher, or
//! both handles being dropped, still ends the worker, whose drop releases whatever is held.

mod config;
mod wayland;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use crosspane_platform::{IoGate, KeyInjector, PlatformError, PointerInjector};
use crosspane_types::geom::PointDevice;
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollDelta};

use super::ipc::HyprIpc;

// Leave a little scheduling margin inside the frozen 50 ms call limit.
const CALL_BUDGET: Duration = Duration::from_millis(45);
const CONNECT_BUDGET: Duration = Duration::from_secs(2);

/// A physical-key injection handle. Its peer uses the same connection and Wayland seat.
#[derive(Debug)]
pub struct HyprlandKeyInjector {
    shared: Arc<Handle>,
}

/// An output-bound pointer injection handle, sharing the keyboard's seat and connection.
#[derive(Debug)]
pub struct HyprlandPointerInjector {
    shared: Arc<Handle>,
}

#[derive(Debug)]
struct Handle {
    commands: SyncSender<Command>,
    locks: config::LockReader,
    key_alive: Arc<AtomicBool>,
    pointer_alive: Arc<AtomicBool>,
}

enum Action {
    Key(HidUsage, bool),
    SetLocks(LockKeys, LockKeys),
    ReleaseKeys,
    RecoverKeys(Vec<HidUsage>),
    Move(DisplayId, PointDevice),
    Button(MouseButton, bool),
    Scroll(ScrollDelta),
    ReleaseButtons,
    RecoverButtons(Vec<MouseButton>),
    DropKeys,
    DropPointers,
}

struct Command {
    action: Action,
    deadline: Instant,
    reply: SyncSender<Result<(), PlatformError>>,
}

impl Handle {
    fn call(&self, action: Action, deadline: Instant) -> Result<(), PlatformError> {
        let (tx, rx) = mpsc::sync_channel(1);
        match self.commands.try_send(Command {
            action,
            deadline,
            reply: tx,
        }) {
            Ok(()) => receive(&rx, deadline)?,
            Err(TrySendError::Full(_)) => Err(PlatformError::Timeout),
            Err(TrySendError::Disconnected(_)) => Err(backend("injection connection lost")),
        }
    }
}

fn receive<T>(rx: &Receiver<T>, deadline: Instant) -> Result<T, PlatformError> {
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|e| match e {
            mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
            mpsc::RecvTimeoutError::Disconnected => backend("injection worker stopped"),
        })
}

fn backend(message: &str) -> PlatformError {
    PlatformError::Backend(message.into())
}

/// Connect to `$WAYLAND_DISPLAY`, uploading one RMLVO keymap to one virtual keyboard and creating
/// one virtual pointer for each known output. Both handles share the connection; output hotplug
/// updates their pointer devices, and changed RMLVO causes a new keymap upload.
pub fn connect(
    gate: Arc<IoGate>,
    ipc: HyprIpc,
) -> Result<(HyprlandKeyInjector, HyprlandPointerInjector), PlatformError> {
    let deadline = Instant::now() + CONNECT_BUDGET;
    let (commands, rx) = mpsc::sync_channel(32);
    let (ready, initialized) = mpsc::sync_channel(1);
    let key_alive = Arc::new(AtomicBool::new(true));
    let pointer_alive = Arc::new(AtomicBool::new(true));
    let keys = key_alive.clone();
    let pointers = pointer_alive.clone();
    let worker_ipc = ipc.clone();
    std::thread::Builder::new()
        .name("hypr-inject".into())
        .spawn(move || {
            let result = (|| {
                let config = config::read(&worker_ipc, None, deadline)?;
                let previous = config.keyboard_addresses.clone();
                let source = wayland::Source::new(gate, config, deadline)?;
                let name = config::own_keyboard_name(&worker_ipc, &previous)?;
                let watcher =
                    config::Watcher::new(worker_ipc, name.clone(), source.config_epoch())?;
                Ok((source, watcher, name))
            })();
            match result {
                Ok((source, watcher, name)) => {
                    if ready.send(Ok(name)).is_ok() {
                        wayland::run(source, rx, keys, pointers, watcher);
                    }
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            }
        })
        .map_err(|_| backend("could not start injection worker"))?;
    let name = receive(&initialized, deadline)??;
    let shared = Arc::new(Handle {
        commands,
        locks: config::LockReader::new(ipc, name)?,
        key_alive,
        pointer_alive,
    });
    Ok((
        HyprlandKeyInjector {
            shared: shared.clone(),
        },
        HyprlandPointerInjector { shared },
    ))
}

impl KeyInjector for HyprlandKeyInjector {
    fn key(&mut self, usage: HidUsage, down: bool) -> Result<(), PlatformError> {
        self.shared
            .call(Action::Key(usage, down), Instant::now() + CALL_BUDGET)
    }

    fn lock_keys(&self) -> Result<LockKeys, PlatformError> {
        self.shared.locks.read(Instant::now() + CALL_BUDGET)
    }

    fn set_lock_keys(&mut self, wanted: LockKeys) -> Result<(), PlatformError> {
        let deadline = Instant::now() + CALL_BUDGET;
        let current = self.shared.locks.read(deadline)?;
        self.shared
            .call(Action::SetLocks(current, wanted), deadline)
    }

    fn release_all(&mut self) -> Result<(), PlatformError> {
        self.shared
            .call(Action::ReleaseKeys, Instant::now() + CALL_BUDGET)
    }

    fn recover_keys(&mut self, keys: &[HidUsage]) -> Result<(), PlatformError> {
        self.shared.call(
            Action::RecoverKeys(keys.to_vec()),
            Instant::now() + CALL_BUDGET,
        )
    }
}

impl PointerInjector for HyprlandPointerInjector {
    fn move_to(&mut self, display: DisplayId, position: PointDevice) -> Result<(), PlatformError> {
        self.shared.call(
            Action::Move(display, position),
            Instant::now() + CALL_BUDGET,
        )
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        self.shared
            .call(Action::Button(button, down), Instant::now() + CALL_BUDGET)
    }

    fn scroll(&mut self, delta: ScrollDelta) -> Result<(), PlatformError> {
        self.shared
            .call(Action::Scroll(delta), Instant::now() + CALL_BUDGET)
    }

    fn release_all(&mut self) -> Result<(), PlatformError> {
        self.shared
            .call(Action::ReleaseButtons, Instant::now() + CALL_BUDGET)
    }

    fn recover_buttons(&mut self, buttons: &[MouseButton]) -> Result<(), PlatformError> {
        self.shared.call(
            Action::RecoverButtons(buttons.to_vec()),
            Instant::now() + CALL_BUDGET,
        )
    }
}

impl Drop for HyprlandKeyInjector {
    fn drop(&mut self) {
        let deadline = Instant::now() + CALL_BUDGET;
        self.shared.key_alive.store(false, Ordering::Release);
        let _ = self.shared.call(Action::DropKeys, deadline);
    }
}

impl Drop for HyprlandPointerInjector {
    fn drop(&mut self) {
        let deadline = Instant::now() + CALL_BUDGET;
        self.shared.pointer_alive.store(false, Ordering::Release);
        let _ = self.shared.call(Action::DropPointers, deadline);
    }
}
