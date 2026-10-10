//! The worker thread: it owns the connection, runs the handles' commands in order, applies what
//! the compositor sends, and keeps the gate watch while anything is held.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use crosspane_platform::{IoGate, PlatformError};
use crosspane_types::input::LockKeys;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use super::conn::{Conn, no_session};
use super::map::ScrollPlan;
use super::regions::RegionRect;

/// How long the handshake with a new EIS socket may take.
pub(super) const HANDSHAKE_BUDGET: Duration = Duration::from_secs(2);
/// How long a connection being replaced or closed has to write out its releases.
const LEAVE_BUDGET: Duration = Duration::from_millis(100);
/// How often the gate is looked at while something is held.
const TICK: Duration = Duration::from_millis(10);

/// State shared between the handles and the worker.
#[derive(Debug)]
pub(super) struct Inner {
    /// A keyboard and an absolute pointer are resumed.
    pub(super) live: AtomicBool,
    /// The latest Caps/Num Lock state, packed by [`pack_locks`].
    pub(super) locks: AtomicU8,
    /// The last handle is gone: end the connection and stop.
    pub(super) closing: AtomicBool,
    /// An eventfd the handles write after queueing a command.
    wake: OwnedFd,
}

impl Inner {
    pub(super) fn new(wake: OwnedFd) -> Self {
        Inner {
            live: AtomicBool::new(false),
            locks: AtomicU8::new(0),
            closing: AtomicBool::new(false),
            wake,
        }
    }

    pub(super) fn wake(&self) {
        // A full counter or a closed fd leaves the worker to its next tick or socket event.
        let _ = rustix::io::write(&self.wake, &1u64.to_ne_bytes());
    }

    fn drain(&self) {
        let mut counter = [0u8; 8];
        let _ = rustix::io::read(&self.wake, &mut counter);
    }

    fn publish(&self, conn: Option<&Conn>) {
        self.live
            .store(conn.is_some_and(Conn::is_live), Ordering::Release);
        self.locks.store(
            conn.map_or(0, |c| pack_locks(c.lock_keys())),
            Ordering::Release,
        );
    }
}

/// Caps and Num Lock as four bits (known, on) each.
pub(super) fn pack_locks(locks: LockKeys) -> u8 {
    let bits = |value: Option<bool>| match value {
        None => 0u8,
        Some(false) => 0b01,
        Some(true) => 0b11,
    };
    bits(locks.caps_lock) | (bits(locks.num_lock) << 2)
}

pub(super) fn unpack_locks(packed: u8) -> LockKeys {
    let value = |bits: u8| match bits & 0b11 {
        0b01 => Some(false),
        0b11 => Some(true),
        _ => None,
    };
    LockKeys {
        caps_lock: value(packed),
        num_lock: value(packed >> 2),
        scroll_lock: None,
    }
}

pub(super) enum Action {
    Attach(OwnedFd),
    Detach,
    Key {
        code: u16,
        down: bool,
    },
    SetLocks(LockKeys),
    ReleaseKeys,
    RecoverKeys(Vec<u16>),
    /// Absolute motion to a logical point on the display whose logical rectangle is `display`.
    Move {
        x: f64,
        y: f64,
        display: RegionRect,
    },
    Button {
        code: u32,
        down: bool,
    },
    Scroll(ScrollPlan),
    ReleaseButtons,
    RecoverButtons(Vec<u32>),
    /// Make the worker fail, to see what the handles report afterwards.
    #[cfg(test)]
    Panic,
}

pub(super) struct Command {
    pub(super) action: Action,
    pub(super) deadline: Instant,
    pub(super) reply: SyncSender<Result<(), PlatformError>>,
}

pub(super) fn spawn(
    gate: Arc<IoGate>,
    inner: Arc<Inner>,
    commands: Receiver<Command>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("eis-inject".into())
        .spawn(move || run(&gate, &inner, &commands))
        .map(drop)
}

/// On the way out of [`run`], however that happens (a panic included), the source stops being
/// live and reports no lock state: with the worker gone nothing can be injected, and `is_live` and
/// `lock_keys` read atomics that only the worker updates.
struct Retire<'a>(&'a Inner);

impl Drop for Retire<'_> {
    fn drop(&mut self) {
        self.0.live.store(false, Ordering::Release);
        self.0.locks.store(0, Ordering::Release);
    }
}

fn run(gate: &Arc<IoGate>, inner: &Inner, commands: &Receiver<Command>) {
    // Declared before the connection, so it drops after the connection has been closed.
    let _retire = Retire(inner);
    let mut conn: Option<Conn> = None;
    loop {
        inner.drain();
        if let Some(c) = conn.as_mut() {
            c.pump();
        }
        loop {
            match commands.try_recv() {
                Ok(command) => {
                    let result = execute(gate, &mut conn, command.action, command.deadline);
                    let _ = command.reply.try_send(result);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    leave(&mut conn);
                    inner.publish(None);
                    return;
                }
            }
        }
        if inner.closing.load(Ordering::Acquire) {
            leave(&mut conn);
            inner.publish(None);
            return;
        }
        if let Some(c) = conn.as_mut() {
            c.retry_flush();
            c.tick();
        }
        if conn.as_ref().is_some_and(Conn::is_dead) {
            tracing::debug!("EIS connection dropped");
            conn = None;
        }
        inner.publish(conn.as_ref());
        wait(inner, conn.as_ref());
    }
}

/// End a connection, releasing what it held.
fn leave(conn: &mut Option<Conn>) {
    if let Some(mut old) = conn.take() {
        old.shutdown(LEAVE_BUDGET);
    }
}

fn execute(
    gate: &Arc<IoGate>,
    conn: &mut Option<Conn>,
    action: Action,
    deadline: Instant,
) -> Result<(), PlatformError> {
    // Releases don't wait on a deadline; a connection that is gone has nothing held.
    match action {
        Action::Attach(fd) => {
            leave(conn);
            let fresh = Conn::connect(fd, gate.clone(), HANDSHAKE_BUDGET)?;
            *conn = Some(fresh);
            Ok(())
        }
        Action::Detach => {
            leave(conn);
            Ok(())
        }
        Action::Key { code, down: true } => with(conn, |c| c.press_key(code, deadline)),
        Action::Key { code, down: false } => release(conn, |c| c.release_key(code, deadline)),
        Action::SetLocks(wanted) => with(conn, |c| c.set_locks(wanted, deadline)),
        Action::ReleaseKeys => release(conn, |c| c.release_keys(deadline)),
        Action::RecoverKeys(codes) => release(conn, |c| c.recover_keys(&codes, deadline)),
        Action::Move { x, y, display } => with(conn, |c| c.move_to(x, y, &display, deadline)),
        Action::Button { code, down: true } => with(conn, |c| c.press_button(code, deadline)),
        Action::Button { code, down: false } => release(conn, |c| c.release_button(code, deadline)),
        Action::Scroll(plan) => {
            if plan.motion.is_some() {
                with(conn, |c| c.scroll(plan, deadline))
            } else {
                release(conn, |c| c.scroll(plan, deadline))
            }
        }
        Action::ReleaseButtons => release(conn, |c| c.release_buttons(deadline)),
        Action::RecoverButtons(codes) => release(conn, |c| c.recover_buttons(&codes, deadline)),
        #[cfg(test)]
        Action::Panic => panic!("test: the EIS worker is made to fail"),
    }
}

/// An input request: it needs a connection.
fn with(
    conn: &mut Option<Conn>,
    op: impl FnOnce(&mut Conn) -> Result<(), PlatformError>,
) -> Result<(), PlatformError> {
    match conn.as_mut() {
        Some(c) => op(c),
        None => Err(no_session()),
    }
}

/// A release: with no connection there is nothing held, which is a success.
fn release(
    conn: &mut Option<Conn>,
    op: impl FnOnce(&mut Conn) -> Result<(), PlatformError>,
) -> Result<(), PlatformError> {
    match conn.as_mut() {
        Some(c) => op(c),
        None => Ok(()),
    }
}

/// Sleep until a command, a socket event, or (while something is held) the next gate look.
fn wait(inner: &Inner, conn: Option<&Conn>) {
    let tick = Timespec {
        tv_sec: 0,
        tv_nsec: i64::from(TICK.subsec_nanos()),
    };
    let timeout = conn.is_some_and(Conn::needs_tick).then_some(&tick);
    let result = match conn {
        Some(c) => {
            let socket = if c.wants_write() {
                PollFlags::IN | PollFlags::OUT
            } else {
                PollFlags::IN
            };
            let mut fds = [
                PollFd::new(&inner.wake, PollFlags::IN),
                PollFd::new(c.context(), socket),
            ];
            poll(&mut fds, timeout)
        }
        None => {
            let mut fds = [PollFd::new(&inner.wake, PollFlags::IN)];
            poll(&mut fds, timeout)
        }
    };
    // A signal or a transient poll error just goes round again.
    let _ = result;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_state_survives_packing() {
        for caps in [None, Some(false), Some(true)] {
            for num in [None, Some(false), Some(true)] {
                let locks = LockKeys {
                    caps_lock: caps,
                    num_lock: num,
                    scroll_lock: None,
                };
                assert_eq!(unpack_locks(pack_locks(locks)), locks);
            }
        }
        assert_eq!(unpack_locks(0), LockKeys::default());
    }

    #[test]
    fn a_worker_that_ends_by_panicking_is_no_longer_live() {
        let wake = rustix::event::eventfd(
            0,
            rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
        )
        .unwrap();
        let inner = Arc::new(Inner::new(wake));
        inner.live.store(true, Ordering::Release);
        inner.locks.store(
            pack_locks(LockKeys {
                caps_lock: Some(true),
                num_lock: Some(false),
                scroll_lock: None,
            }),
            Ordering::Release,
        );
        let worker = {
            let inner = inner.clone();
            std::thread::spawn(move || {
                let _retire = Retire(&inner);
                panic!("test: the worker fails");
            })
        };
        assert!(worker.join().is_err());
        assert!(!inner.live.load(Ordering::Acquire));
        assert_eq!(
            unpack_locks(inner.locks.load(Ordering::Acquire)),
            LockKeys::default()
        );
    }
}
