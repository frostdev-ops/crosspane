//! The RemoteDesktop portal session that authorizes input injection (WP-G1.3).
//!
//! One worker thread owns the session through `ashpd` (async-io executor, driven with
//! `async_io::block_on` or an equivalent on that thread only). It:
//!
//! 1. creates a RemoteDesktop session, selects the keyboard and pointer device types with
//!    `PersistMode::ExplicitlyRevoked` and the restore token read from `token_path` (if any);
//! 2. starts it, which may show the consent dialog the first time; the start waits as long as the
//!    user takes, but never on a trait call;
//! 3. writes the rotated restore token the start returns to `token_path` (mode 0600, written to a
//!    temporary file and renamed; a missing or unreadable token just means consent is asked again);
//! 4. calls `ConnectToEIS` and hands the socket over through [`RemoteDesktopSession::take_eis`];
//! 5. watches the session's `Closed` signal and the portal's bus name. Either one ends the epoch.
//!
//! **Epochs.** Every successful start is a new epoch (1, 2, …). A result that arrives after
//! [`RemoteDesktopSession::close`] or after its epoch ended is stale and dropped, never reported
//! as `Active`. A denied or cancelled dialog is [`SessionStatus::Denied`] and is not retried
//! automatically; [`RemoteDesktopSession::restart`] asks again (the tray's "Allow remote input").
//!
//! *Amended 2026-10-09 (lead).* The session's `Closed` signal means the user or the desktop ended
//! remote control, e.g. with the Stop button of GNOME's remote-desktop indicator. That is
//! [`SessionStatus::Closed`], and it stays `Closed` until `restart`: re-arming on its own with the
//! stored token would override the user's explicit stop. Only the portal itself going away (its
//! bus name loses its owner or changes hands: the portal restarted or crashed) also ends the epoch
//! with `Closed`, and then the worker retries once, after 1 s, with the stored token (silent when
//! the desktop still honours it), and otherwise stays `Closed` until `restart`.
//! (The earlier text retried after either cause.)
//!
//! Portal closure is authorization loss, not lock evidence: the I/O gate is the session backend's
//! (logind) business, not this module's.
//!
//! # Implementation
//!
//! The worker thread runs the attempts with `zbus::block_on`. The handle never waits for it:
//! `close` and `restart` change a small shared state (a generation counter, a closed flag, the
//! stored EIS socket) under one mutex and wake the worker, so they are safe to call from the status
//! callback. Every await in the worker is raced against that state, so a close or restart drops
//! whatever is in flight at once, and the worker publishes an epoch's `Active` status and socket in
//! one step that fails if the state changed meanwhile (the stale-result rule).
//!
//! Each attempt uses its own session-bus connection, so a restarted bus is picked up by the next
//! attempt. The epoch/retry policy is the pure state machine in `lifecycle`; the restore-token file
//! is `token`.
//!
//! A start attempt that ends because the user said no, or the desktop granted less than keyboard
//! and pointer, is `Denied`. Any other failure to get a session (no portal, an old portal without
//! `ConnectToEIS`, a failing call, no session bus) is `Unavailable`. A closed session is only ever
//! restarted by an explicit `restart`; so is a lost portal once its one automatic retry (which
//! keeps the status `Closed` while it runs and if it fails) has been spent.

mod lifecycle;
mod sleep;
mod token;

#[cfg(test)]
mod fake_portal;

use std::cell::Cell;
use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::os::fd::OwnedFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ashpd::PortalError;
use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::{PersistMode, Session};
use crosspane_platform::PlatformError;
use zbus::export::futures_core::Stream;
use zbus::names::UniqueName;
use zbus::zvariant::OwnedValue;

use lifecycle::{Event, Failure, Lifecycle, Phase, closed_status};
use sleep::{Sleep, timeout};

/// Wait before the automatic retry, so a restarting portal can register its interfaces again.
const RETRY_DELAY: Duration = Duration::from_secs(1);
/// How long closing a portal session may take before the worker gives up on the reply.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long dropping the handle waits for the worker before detaching it.
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// What the session is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDesktopConfig {
    /// Where the restore token lives, e.g. `<state_dir>/portal-remote-desktop.token`.
    pub token_path: PathBuf,
}

/// The session's state, as reported to the status callback and by [`RemoteDesktopSession::status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    /// Starting: creating the session or waiting for the user's consent.
    Pending,
    /// Started with keyboard and pointer granted; its EIS socket is (or was) available.
    Active { epoch: u64 },
    /// The user denied or cancelled the dialog, or the desktop granted less than keyboard and
    /// pointer. Not retried until `restart`.
    Denied,
    /// The epoch's session ended (revoked, portal gone, `close`).
    Closed { epoch: u64 },
    /// No RemoteDesktop portal (or a version without `ConnectToEIS`) on this desktop.
    Unavailable,
}

/// Called on the worker thread on every status change, in order. Must not block.
pub type StatusCallback = Arc<dyn Fn(SessionStatus) + Send + Sync>;

/// A handle to the session worker. Dropping it closes the session and joins the worker (bounded).
#[derive(Debug)]
pub struct RemoteDesktopSession {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl RemoteDesktopSession {
    /// Start the worker and return immediately (within 2 s). The first status is `Pending`.
    pub fn spawn(
        config: RemoteDesktopConfig,
        on_status: StatusCallback,
    ) -> Result<RemoteDesktopSession, PlatformError> {
        Self::spawn_on(config, on_status, None)
    }

    /// [`spawn`](Self::spawn) on a given bus address instead of the session bus. Tests use it to
    /// stay away from the real portal.
    fn spawn_on(
        config: RemoteDesktopConfig,
        on_status: StatusCallback,
        bus_address: Option<String>,
    ) -> Result<RemoteDesktopSession, PlatformError> {
        let shared = Arc::new(Shared::new());
        let worker = Worker {
            shared: Arc::clone(&shared),
            token_path: config.token_path,
            on_status,
            bus_address,
            seen: Cell::new(0),
        };
        let handle = thread::Builder::new()
            .name("crosspane-portal-session".to_owned())
            .spawn(move || worker.run())
            .map_err(|error| {
                PlatformError::Backend(format!("cannot start the RemoteDesktop worker: {error}"))
            })?;
        Ok(RemoteDesktopSession {
            shared,
            worker: Some(handle),
        })
    }

    /// The current status.
    pub fn status(&self) -> SessionStatus {
        self.shared.lock().status
    }

    /// The EIS socket of the active epoch. Each epoch's socket is handed out exactly once; later
    /// calls in the same epoch, and calls while not `Active`, return `None`.
    pub fn take_eis(&self) -> Option<(u64, OwnedFd)> {
        let mut state = self.shared.lock();
        let SessionStatus::Active { epoch } = state.status else {
            return None;
        };
        if matches!(&state.eis, Some((stored, _)) if *stored == epoch) {
            state.eis.take()
        } else {
            None
        }
    }

    /// Close the current session (if any) and start a new one, which may ask the user again.
    pub fn restart(&self) -> Result<(), PlatformError> {
        let waker = {
            let mut state = self.shared.lock();
            if state.closed || state.exited {
                return Err(PlatformError::Backend(
                    "the RemoteDesktop session is closed".to_owned(),
                ));
            }
            state.generation += 1;
            // The old epoch's socket must not outlive the request.
            state.eis = None;
            if let SessionStatus::Active { epoch } = state.status {
                state.status = SessionStatus::Closed { epoch };
            }
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    /// Close the session for good. Idempotent; later results are stale.
    pub fn close(&self) {
        let waker = {
            let mut state = self.shared.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.generation += 1;
            state.eis = None;
            state.status = closed_status(state.status, state.last_epoch);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl Drop for RemoteDesktopSession {
    fn drop(&mut self) {
        self.close();
        let Some(handle) = self.worker.take() else {
            return;
        };
        // A status callback that drops the last handle runs on the worker itself.
        if handle.thread().id() == thread::current().id() {
            return;
        }
        let deadline = Instant::now() + JOIN_TIMEOUT;
        let mut state = self.shared.lock();
        while !state.exited {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self
                .shared
                .exited
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let exited = state.exited;
        drop(state);
        if exited {
            if handle.join().is_err() {
                tracing::warn!("the RemoteDesktop worker panicked");
            }
        } else {
            tracing::warn!("the RemoteDesktop worker did not stop in time; detaching it");
        }
    }
}

// The handle is shared between the agent's threads.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RemoteDesktopSession>();
};

/// What the handle and the worker share.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    /// Signalled when the worker exits.
    exited: Condvar,
}

#[derive(Debug)]
struct State {
    /// The status handles see. Written by the worker, except that `close` and `restart` close an
    /// active epoch at once, together with dropping its socket.
    status: SessionStatus,
    /// Bumped by every `restart` and `close`; the worker compares it with the one it last handled.
    generation: u64,
    /// `close` was called.
    closed: bool,
    /// The worker has stopped (or died).
    exited: bool,
    /// The last epoch handed out; 0 before the first.
    last_epoch: u64,
    /// The active epoch's socket until `take_eis` takes it.
    eis: Option<(u64, OwnedFd)>,
    /// The worker's waker, for `close` and `restart` to wake it.
    waker: Option<Waker>,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            state: Mutex::new(State {
                status: SessionStatus::Pending,
                generation: 0,
                closed: false,
                exited: false,
                last_epoch: 0,
                eis: None,
                waker: None,
            }),
            exited: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The change the handle asked for since the worker last handled one, if any. Marks a restart
    /// as handled (a close stays pending: it is final).
    fn change_locked(state: &State, seen: &Cell<u64>) -> Option<Event> {
        if state.closed {
            Some(Event::Close)
        } else if state.generation != seen.get() {
            seen.set(state.generation);
            Some(Event::Restart)
        } else {
            None
        }
    }

    /// [`change_locked`](Self::change_locked) for a poll: if there is none, remember `waker` so
    /// the next `close` or `restart` wakes the worker.
    fn poll_change(&self, seen: &Cell<u64>, waker: &Waker) -> Option<Event> {
        let mut state = self.lock();
        if let Some(event) = Self::change_locked(&state, seen) {
            return Some(event);
        }
        if !matches!(&state.waker, Some(old) if old.will_wake(waker)) {
            state.waker = Some(waker.clone());
        }
        None
    }

    fn take_change(&self, seen: &Cell<u64>) -> Option<Event> {
        Self::change_locked(&self.lock(), seen)
    }

    /// Publish an epoch: its socket and `Active` status become visible together, unless a close or
    /// restart arrived since the worker last looked (the result is stale and `eis` is dropped).
    fn publish_active(&self, epoch: u64, eis: OwnedFd, seen: &Cell<u64>) -> bool {
        let mut state = self.lock();
        if state.closed || state.generation != seen.get() {
            return false;
        }
        state.eis = Some((epoch, eis));
        state.last_epoch = epoch;
        state.status = SessionStatus::Active { epoch };
        true
    }

    /// The epoch is over: drop its socket if nobody took it.
    fn end_epoch(&self) {
        self.lock().eis = None;
    }
}

/// Marks the worker as exited when it ends, even by a panic.
struct ExitGuard(Arc<Shared>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0.lock().exited = true;
        self.0.exited.notify_all();
    }
}

/// Why a guarded step stopped early.
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// `close` or `restart` was called.
    Event(Event),
    /// The portal emitted `Closed` for our session.
    SessionClosed,
    /// The portal's bus name changed owner or vanished.
    PortalLost,
}

impl Stop {
    /// The session no longer exists on the portal's side.
    fn session_gone(self) -> bool {
        matches!(self, Stop::SessionClosed | Stop::PortalLost)
    }

    /// The lifecycle event this stop is, depending on whether the epoch had begun.
    fn into_event(self, active: bool) -> Event {
        match self {
            Stop::Event(event) => event,
            Stop::SessionClosed if active => Event::Revoked,
            Stop::SessionClosed => Event::Failed(Failure::Denied),
            Stop::PortalLost if active => Event::PortalLost,
            Stop::PortalLost => Event::Failed(Failure::Unavailable),
        }
    }
}

type ClosedStream<'a> = Pin<Box<dyn Stream<Item = HashMap<String, OwnedValue>> + 'a>>;
type OwnerStream = Pin<Box<dyn Stream<Item = Option<UniqueName<'static>>>>>;

/// The portal-side events an attempt watches once it has them. `closed` borrows the session.
#[derive(Default)]
struct Streams<'a> {
    closed: Option<ClosedStream<'a>>,
    owner: Option<OwnerStream>,
}

/// The worker's state. Everything here is used on the worker thread only.
struct Worker {
    shared: Arc<Shared>,
    token_path: PathBuf,
    on_status: StatusCallback,
    /// `None`: the session bus.
    bus_address: Option<String>,
    /// The generation of the last `restart` the worker handled.
    seen: Cell<u64>,
}

impl Worker {
    fn run(self) {
        let _exit = ExitGuard(Arc::clone(&self.shared));
        self.notify(SessionStatus::Pending);
        zbus::block_on(self.drive());
    }

    /// Run the lifecycle until it is finished.
    async fn drive(&self) {
        let mut lifecycle = Lifecycle::new();
        loop {
            let event = match lifecycle.phase() {
                Phase::Finished => return,
                Phase::Starting { silent } => self.attempt(&mut lifecycle, silent).await,
                Phase::Idle => self.wait_change().await,
                // An attempt only returns once its epoch is over. If it ever did not, fail closed
                // without re-arming.
                Phase::Active { .. } => Event::Revoked,
            };
            for status in lifecycle.step(event) {
                self.report(status);
            }
        }
    }

    /// Record `status` for handles and tell the callback, with no lock held. After `close` the
    /// handle-visible status is final, and `Active` is published together with its socket.
    fn report(&self, status: SessionStatus) {
        {
            let mut state = self.shared.lock();
            if !state.closed && !matches!(status, SessionStatus::Active { .. }) {
                state.status = status;
            }
        }
        self.notify(status);
    }

    fn notify(&self, status: SessionStatus) {
        if catch_unwind(AssertUnwindSafe(|| (self.on_status)(status))).is_err() {
            tracing::warn!("the RemoteDesktop status callback panicked");
        }
    }

    /// Wait for a `restart` or `close` while idle.
    async fn wait_change(&self) -> Event {
        poll_fn(|cx| match self.shared.poll_change(&self.seen, cx.waker()) {
            Some(event) => Poll::Ready(event),
            None => Poll::Pending,
        })
        .await
    }

    /// Poll `future` unless a `close`/`restart` or a portal-side event comes first, and in that
    /// case drop it. The handle's requests come first, so a result that is ready at the same
    /// moment as a close is dropped; a ready result comes before a portal-side `Closed`, so a
    /// cancelled dialog is still its response and not its session's closing.
    async fn watch<F: Future>(
        &self,
        streams: &mut Streams<'_>,
        future: F,
    ) -> Result<F::Output, Stop> {
        let mut future = std::pin::pin!(future);
        poll_fn(|cx| {
            if let Some(event) = self.shared.poll_change(&self.seen, cx.waker()) {
                return Poll::Ready(Err(Stop::Event(event)));
            }
            if let Poll::Ready(value) = future.as_mut().poll(cx) {
                return Poll::Ready(Ok(value));
            }
            if let Some(stream) = streams.closed.as_mut()
                && stream.as_mut().poll_next(cx).is_ready()
            {
                return Poll::Ready(Err(Stop::SessionClosed));
            }
            if let Some(stream) = streams.owner.as_mut()
                && stream.as_mut().poll_next(cx).is_ready()
            {
                return Poll::Ready(Err(Stop::PortalLost));
            }
            Poll::Pending
        })
        .await
    }

    /// One start attempt and, if it succeeds, the epoch that follows, until it ends. Returns the
    /// event that ended it.
    async fn attempt(&self, lifecycle: &mut Lifecycle, silent: bool) -> Event {
        let mut session = None;
        let mut gone = false;
        let event = self
            .run_attempt(lifecycle, silent, &mut session, &mut gone)
            .await;
        // The epoch is over: nobody gets its socket any more.
        self.shared.end_epoch();
        if let Some(session) = session
            && !gone
        {
            // Closing also dismisses a consent dialog that is still up. A session the portal
            // already closed, or a portal that is gone, needs no call (it could even restart the
            // portal).
            close_session(&session).await;
        }
        event
    }

    async fn run_attempt(
        &self,
        lifecycle: &mut Lifecycle,
        silent: bool,
        slot: &mut Option<Session<RemoteDesktop>>,
        gone: &mut bool,
    ) -> Event {
        let mut streams = Streams::default();

        // Await `$future` guarded by close/restart and the portal-side events, leaving the
        // attempt with the right event if one of those comes first or the call fails.
        macro_rules! step {
            ($future:expr) => {
                match self.watch(&mut streams, $future).await {
                    Err(stop) => {
                        *gone = stop.session_gone();
                        return stop.into_event(false);
                    }
                    Ok(Err(error)) => {
                        let error: ashpd::Error = error.into();
                        return self.failed(&error);
                    }
                    Ok(Ok(value)) => value,
                }
            };
        }

        if silent {
            match self.watch(&mut streams, Sleep::new(RETRY_DELAY)).await {
                Ok(()) => {}
                Err(stop) => return stop.into_event(false),
            }
        }
        // Read at each attempt: the last one wrote the rotated token.
        let restore_token = token::read(&self.token_path);

        let connection = step!(self.connect());
        if self.bus_address.is_none() {
            super::register_on(&connection).await;
        }
        let remote = step!(RemoteDesktop::with_connection(connection));
        if remote.version() < 2 {
            // ConnectToEIS (and persistence) arrived in version 2.
            tracing::warn!(
                version = remote.version(),
                "the RemoteDesktop portal is missing or too old (no ConnectToEIS)"
            );
            return Event::Failed(Failure::Unavailable);
        }
        // Watch the portal's name before creating anything on it.
        streams.owner = Some(Box::pin(step!(remote.receive_owner_changed())));

        *slot = Some(step!(remote.create_session(Default::default())));
        let Some(session) = slot.as_ref() else {
            return Event::Failed(Failure::Unavailable);
        };
        // Subscribe before `Start`: the dialog can end the session.
        streams.closed = Some(Box::pin(step!(session.receive_closed())));

        let options = SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(restore_token.as_deref());
        let selected = step!(remote.select_devices(session, options));
        if let Err(error) = selected.response() {
            return self.failed(&error);
        }

        tracing::info!("asking the RemoteDesktop portal to start the session");
        let started = step!(remote.start(session, None, StartOptions::default()));
        let granted = match started.response() {
            Ok(granted) => granted,
            Err(error) => return self.failed(&error),
        };
        let devices = granted.devices();
        if !devices.contains(DeviceType::Keyboard) || !devices.contains(DeviceType::Pointer) {
            tracing::warn!(
                keyboard = devices.contains(DeviceType::Keyboard),
                pointer = devices.contains(DeviceType::Pointer),
                "the desktop granted less than keyboard and pointer"
            );
            return Event::Failed(Failure::Denied);
        }
        match granted.restore_token() {
            Some(rotated) => {
                if let Err(error) = token::write(&self.token_path, rotated) {
                    tracing::warn!(
                        %error,
                        "cannot store the portal restore token; consent will be asked again"
                    );
                }
            }
            None => tracing::info!("the portal returned no restore token"),
        }

        let eis = step!(remote.connect_to_eis(session, ConnectToEISOptions::default()));

        let epoch = lifecycle.next_epoch();
        if !self.shared.publish_active(epoch, eis, &self.seen) {
            // A close or restart arrived while the session was starting: it is stale.
            return self
                .shared
                .take_change(&self.seen)
                .unwrap_or(Event::Restart);
        }
        for status in lifecycle.step(Event::Granted) {
            self.report(status);
        }
        tracing::info!(epoch, "the RemoteDesktop session is active");

        // The epoch lasts until the session closes, the portal goes away, or the handle asks.
        match self.watch(&mut streams, std::future::pending::<()>()).await {
            Err(stop) => {
                *gone = stop.session_gone();
                match stop {
                    Stop::SessionClosed => tracing::info!(
                        epoch,
                        "the portal closed the RemoteDesktop session (remote control was stopped)"
                    ),
                    Stop::PortalLost => {
                        tracing::warn!(epoch, "the RemoteDesktop portal went away")
                    }
                    Stop::Event(_) => tracing::info!(epoch, "the RemoteDesktop session ended"),
                }
                stop.into_event(true)
            }
            // Unreachable (the future never completes); never re-arm on it.
            Ok(()) => Event::Revoked,
        }
    }

    async fn connect(&self) -> Result<zbus::Connection, zbus::Error> {
        match &self.bus_address {
            None => zbus::Connection::session().await,
            Some(address) => {
                zbus::connection::Builder::address(address.as_str())?
                    .build()
                    .await
            }
        }
    }

    /// Log why an attempt failed and classify it.
    fn failed(&self, error: &ashpd::Error) -> Event {
        let failure = classify(error);
        match failure {
            Failure::Denied => tracing::info!(%error, "the RemoteDesktop request was not granted"),
            Failure::Unavailable => {
                tracing::warn!(%error, "the RemoteDesktop portal is not usable")
            }
        }
        Event::Failed(failure)
    }
}

/// Close the portal session, bounded. Errors only matter for the log.
async fn close_session(session: &Session<RemoteDesktop>) {
    match timeout(session.close(), CLOSE_TIMEOUT).await {
        Some(Ok(())) => {}
        Some(Err(error)) => tracing::debug!(%error, "closing the portal session failed"),
        None => tracing::warn!("closing the portal session timed out"),
    }
}

/// A user's "no" is `Denied`; every other way of not getting a session is `Unavailable`.
fn classify(error: &ashpd::Error) -> Failure {
    match error {
        ashpd::Error::Response(_)
        | ashpd::Error::Portal(PortalError::Cancelled(_) | PortalError::NotAllowed(_)) => {
            Failure::Denied
        }
        _ => Failure::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ashpd::desktop::ResponseError;

    #[test]
    fn denied_and_cancelled_responses_are_denied() {
        for error in [
            ashpd::Error::Response(ResponseError::Cancelled),
            ashpd::Error::Response(ResponseError::Other),
            ashpd::Error::Portal(PortalError::Cancelled(String::new())),
            ashpd::Error::Portal(PortalError::NotAllowed(String::new())),
        ] {
            assert_eq!(classify(&error), Failure::Denied, "{error}");
        }
    }

    #[test]
    fn missing_or_failing_portals_are_unavailable() {
        let missing =
            zbus::names::OwnedInterfaceName::try_from("org.freedesktop.portal.RemoteDesktop")
                .unwrap();
        for error in [
            ashpd::Error::RequiresVersion(2, 1),
            ashpd::Error::PortalNotFound(missing),
            ashpd::Error::NoResponse,
            ashpd::Error::Portal(PortalError::Failed(String::new())),
            ashpd::Error::Zbus(zbus::Error::FDO(Box::new(
                zbus::fdo::Error::ServiceUnknown(String::new()),
            ))),
            ashpd::Error::Zbus(zbus::Error::Failure(String::new())),
        ] {
            assert_eq!(classify(&error), Failure::Unavailable, "{error}");
        }
    }

    #[test]
    fn stops_map_to_events_by_phase() {
        assert_eq!(Stop::Event(Event::Close).into_event(false), Event::Close);
        assert_eq!(Stop::Event(Event::Restart).into_event(true), Event::Restart);
        // The session's `Closed` signal is a revoke (never retried); only the portal's bus name
        // going away is a lost portal (retried once).
        assert_eq!(Stop::SessionClosed.into_event(true), Event::Revoked);
        assert_eq!(Stop::PortalLost.into_event(true), Event::PortalLost);
        assert_eq!(
            Stop::SessionClosed.into_event(false),
            Event::Failed(Failure::Denied)
        );
        assert_eq!(
            Stop::PortalLost.into_event(false),
            Event::Failed(Failure::Unavailable)
        );
        assert!(Stop::SessionClosed.session_gone());
        assert!(Stop::PortalLost.session_gone());
        assert!(!Stop::Event(Event::Close).session_gone());
    }
}
