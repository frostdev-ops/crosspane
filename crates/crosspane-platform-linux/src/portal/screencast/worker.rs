//! The ScreenCast portal session worker (WP-G2.2).
//!
//! One worker thread owns the session through `ashpd`, driven with `zbus::block_on` on that
//! thread only. It follows the RemoteDesktop worker (`portal::session`): the same epoch rules,
//! the same stale-result rule, the same lifecycle state machine (`lifecycle`, shared by path),
//! the same restore-token handling (`token`, a copy). In order it:
//!
//! 1. creates a ScreenCast session, selects every monitor (`SourceType::Monitor`,
//!    `multiple = true`, `CursorMode::Hidden`) with `PersistMode::ExplicitlyRevoked` and the stored
//!    restore token (if any);
//! 2. starts it, which may show the desktop's share-screen dialog the first time; the start waits
//!    as long as the user takes, but never on a trait call;
//! 3. stores the rotated restore token (0600, temporary file + rename; failure just means consent
//!    is asked again);
//! 4. calls `OpenPipeWireRemote` and hands the fd to the PipeWire thread (`Command::Attach`), then
//!    publishes the epoch's streams to the handle;
//! 5. watches the session's `Closed` signal, the portal's bus name and the PipeWire core. When the
//!    epoch ends it tells the PipeWire thread (`Command::Detach`), which ends every stream.
//!
//! **Epochs and endings.** Every successful start is a new epoch (1, 2, ...). A result that
//! arrives after `close`/`restart`, or after its epoch ended, is stale and dropped. The portal's
//! `Closed` signal (the user pressed Stop, or the desktop revoked sharing) ends the epoch's streams
//! with `StreamEndReason::Blocked` ("permission revoked") and stays closed until `restart`: re-arming
//! on its own would override the user's stop. Losing the portal (its bus name changed hands) or the
//! PipeWire core ends them with `Failed` and gets one silent retry with the stored token, then
//! stays closed until `restart`. `restart` and `close` end them with `Failed` and `Requested`.
//!
//! A start attempt that ends because the user said no, or the desktop granted no usable monitor, is
//! `Denied`. Any other failure to get a session (no portal, a failing call, no session bus) is
//! `Unavailable`.

use std::cell::Cell;
use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ashpd::PortalError;
use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use crosspane_platform::{Permission, PlatformError, StreamEndReason};
use zbus::export::futures_core::Stream;
use zbus::names::UniqueName;
use zbus::zvariant::OwnedValue;

use super::SessionStatus;
use super::capture::Command;
use super::lifecycle::{Event, Failure, Lifecycle, Phase, closed_status};
use super::sleep::{Sleep, timeout};
use super::streams::PortalStream;
use super::token;

/// Wait before the automatic retry, so a restarting portal can register its interfaces again.
const RETRY_DELAY: Duration = Duration::from_secs(1);
/// How long closing a portal session may take before the worker gives up on the reply.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long dropping the handle waits for the worker before detaching it.
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// The streams of the active epoch.
#[derive(Clone, Debug)]
pub(super) struct Active {
    pub epoch: u64,
    pub streams: Arc<[PortalStream]>,
}

/// What the handle and the worker share.
#[derive(Debug)]
pub(super) struct Shared {
    state: Mutex<State>,
    /// Signalled when the worker exits.
    exited: Condvar,
}

#[derive(Debug)]
struct State {
    /// The status handles see. Written by the worker, except that `close` and `restart` close an
    /// active epoch at once, together with dropping its streams.
    status: SessionStatus,
    /// Bumped by every `restart` and `close`; the worker compares it with the one it last handled.
    generation: u64,
    /// `close` was called.
    closed: bool,
    /// The worker has stopped (or died).
    exited: bool,
    /// The last epoch handed out; 0 before the first.
    last_epoch: u64,
    /// The active epoch and its streams.
    active: Option<Active>,
    /// The PipeWire thread lost its connection for this epoch.
    core_lost: Option<u64>,
    /// The worker's waker, for `close`, `restart` and `core_lost` to wake it.
    waker: Option<Waker>,
}

impl Shared {
    pub(super) fn new() -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(State {
                status: SessionStatus::Pending,
                generation: 0,
                closed: false,
                exited: false,
                last_epoch: 0,
                active: None,
                core_lost: None,
                waker: None,
            }),
            exited: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The active epoch, or why there is none. Never waits for the user.
    pub(super) fn active(&self) -> Result<Active, PlatformError> {
        let state = self.lock();
        if state.closed || state.exited {
            return Err(PlatformError::Backend(
                "screencast: the capture is shut down".into(),
            ));
        }
        match (state.status, &state.active) {
            (SessionStatus::Active { epoch }, Some(active)) if active.epoch == epoch => {
                if state.core_lost == Some(epoch) {
                    Err(PlatformError::Backend(
                        "screencast: the PipeWire connection was lost".into(),
                    ))
                } else {
                    Ok(active.clone())
                }
            }
            (SessionStatus::Denied, _) => {
                Err(PlatformError::PermissionDenied(Permission::ScreenRecording))
            }
            (SessionStatus::Unavailable, _) => {
                Err(PlatformError::Unsupported("no ScreenCast portal"))
            }
            // Pending (the dialog may be up) or closed: the caller decides about asking again.
            _ => Err(PlatformError::InteractionRequired),
        }
    }

    /// Close the current session (if any) and start a new one, which may ask the user again.
    pub(super) fn restart(&self) -> Result<(), PlatformError> {
        let waker = {
            let mut state = self.lock();
            if state.closed || state.exited {
                return Err(PlatformError::Backend(
                    "screencast: the capture is shut down".into(),
                ));
            }
            state.generation += 1;
            // The old epoch's streams must not be started any more.
            state.active = None;
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
    pub(super) fn close(&self) {
        let waker = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.generation += 1;
            state.active = None;
            state.status = closed_status(state.status, state.last_epoch);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// The PipeWire thread lost its connection for `epoch` (or could not make one). The worker
    /// ends the epoch.
    pub(super) fn core_lost(&self, epoch: u64) {
        let waker = {
            let mut state = self.lock();
            state.core_lost = Some(epoch);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// [`close`](Self::close), then wait (bounded) for the worker to exit and join it.
    pub(super) fn close_and_join(&self, handle: JoinHandle<()>) {
        self.close();
        // A status callback that drops the last handle would run on the worker itself.
        if handle.thread().id() == thread::current().id() {
            return;
        }
        let deadline = Instant::now() + JOIN_TIMEOUT;
        let mut state = self.lock();
        while !state.exited {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self
                .exited
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let exited = state.exited;
        drop(state);
        if exited {
            if handle.join().is_err() {
                tracing::warn!("the ScreenCast worker panicked");
            }
        } else {
            tracing::warn!("the ScreenCast worker did not stop in time; detaching it");
        }
    }

    /// The change the handle asked for since the worker last handled one, if any. Marks a restart
    /// as handled (a close stays pending: it is final).
    fn change_locked(state: &State, seen: &Cell<u64>) -> Option<Stop> {
        if state.closed {
            Some(Stop::Event(Event::Close))
        } else if state.generation != seen.get() {
            seen.set(state.generation);
            Some(Stop::Event(Event::Restart))
        } else if let (Some(lost), Some(active)) = (state.core_lost, &state.active)
            && lost == active.epoch
        {
            Some(Stop::CoreLost)
        } else {
            None
        }
    }

    /// [`change_locked`](Self::change_locked) for a poll: if there is none, remember `waker` so
    /// the next change wakes the worker.
    fn poll_change(&self, seen: &Cell<u64>, waker: &Waker) -> Option<Stop> {
        let mut state = self.lock();
        if let Some(stop) = Self::change_locked(&state, seen) {
            return Some(stop);
        }
        if !matches!(&state.waker, Some(old) if old.will_wake(waker)) {
            state.waker = Some(waker.clone());
        }
        None
    }

    fn take_change(&self, seen: &Cell<u64>) -> Option<Event> {
        match Self::change_locked(&self.lock(), seen) {
            Some(Stop::Event(event)) => Some(event),
            Some(_) => Some(Event::PortalLost),
            None => None,
        }
    }

    /// Publish an epoch's streams, unless a close or restart arrived since the worker last looked
    /// (the result is stale and dropped).
    fn publish_active(&self, epoch: u64, streams: Arc<[PortalStream]>, seen: &Cell<u64>) -> bool {
        let mut state = self.lock();
        if state.closed || state.generation != seen.get() {
            return false;
        }
        state.active = Some(Active { epoch, streams });
        state.last_epoch = epoch;
        state.status = SessionStatus::Active { epoch };
        true
    }

    /// The epoch is over: nobody starts its streams any more.
    fn end_epoch(&self) {
        let mut state = self.lock();
        state.active = None;
        state.core_lost = None;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    /// `close` or `restart` was called.
    Event(Event),
    /// The portal emitted `Closed` for our session.
    SessionClosed,
    /// The portal's bus name changed owner or vanished.
    PortalLost,
    /// The PipeWire thread lost its connection to the server.
    CoreLost,
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
            Stop::PortalLost | Stop::CoreLost if active => Event::PortalLost,
            Stop::PortalLost | Stop::CoreLost => Event::Failed(Failure::Unavailable),
        }
    }
}

/// How the streams of an epoch end when the epoch does.
fn detach_reason(event: Event) -> StreamEndReason {
    match event {
        // Remote sharing was stopped: permission revoked, and nobody may re-arm it silently.
        Event::Revoked => StreamEndReason::Blocked,
        // The owner dropped the capture.
        Event::Close => StreamEndReason::Requested,
        Event::PortalLost | Event::Restart | Event::Failed(_) | Event::Granted => {
            StreamEndReason::Failed
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

/// What one attempt leaves behind for its epilogue.
#[derive(Default)]
struct Attempt {
    /// The portal session, to close.
    session: Option<Session<Screencast>>,
    /// The portal already closed the session or is gone: no close call needed.
    gone: bool,
    /// The epoch whose fd was handed to the PipeWire thread.
    attached: Option<u64>,
}

/// The worker's state. Everything here is used on the worker thread only.
struct Worker {
    shared: Arc<Shared>,
    token_path: PathBuf,
    commands: Sender<Command>,
    /// `None`: the session bus.
    bus_address: Option<String>,
    /// The generation of the last `restart` the worker handled.
    seen: Cell<u64>,
}

/// Start the worker thread. It runs until `Shared::close`.
pub(super) fn spawn(
    shared: Arc<Shared>,
    token_path: PathBuf,
    commands: Sender<Command>,
    bus_address: Option<String>,
) -> Result<JoinHandle<()>, PlatformError> {
    let worker = Worker {
        shared,
        token_path,
        commands,
        bus_address,
        seen: Cell::new(0),
    };
    thread::Builder::new()
        .name("crosspane-portal-screencast".to_owned())
        .spawn(move || worker.run())
        .map_err(|error| {
            PlatformError::Backend(format!("cannot start the ScreenCast worker: {error}"))
        })
}

impl Worker {
    fn run(self) {
        let _exit = ExitGuard(Arc::clone(&self.shared));
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

    /// Record `status` for handles. After `close` the handle-visible status is final, and `Active`
    /// is published together with its streams.
    fn report(&self, status: SessionStatus) {
        let mut state = self.shared.lock();
        if !state.closed && !matches!(status, SessionStatus::Active { .. }) {
            state.status = status;
        }
    }

    /// Wait for a `restart` or `close` while idle.
    async fn wait_change(&self) -> Event {
        poll_fn(|cx| match self.shared.poll_change(&self.seen, cx.waker()) {
            Some(Stop::Event(event)) => Poll::Ready(event),
            // An idle worker has no epoch to lose.
            Some(_) | None => Poll::Pending,
        })
        .await
    }

    /// Poll `future` unless a `close`/`restart`, a PipeWire loss or a portal-side event comes
    /// first, and in that case drop it. The handle's requests come first, so a result that is
    /// ready at the same moment as a close is dropped; a ready result comes before a portal-side
    /// `Closed`, so a cancelled dialog is still its response and not its session's closing.
    async fn watch<F: Future>(
        &self,
        streams: &mut Streams<'_>,
        future: F,
    ) -> Result<F::Output, Stop> {
        let mut future = std::pin::pin!(future);
        poll_fn(|cx| {
            if let Some(stop) = self.shared.poll_change(&self.seen, cx.waker()) {
                return Poll::Ready(Err(stop));
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
        let mut attempt = Attempt::default();
        let event = self.run_attempt(lifecycle, silent, &mut attempt).await;
        // The epoch is over: nobody starts its streams any more, and the PipeWire thread ends the
        // ones that run.
        self.shared.end_epoch();
        if let Some(epoch) = attempt.attached {
            let reason = detach_reason(event);
            if self
                .commands
                .send(Command::Detach { epoch, reason })
                .is_err()
            {
                tracing::warn!("the PipeWire thread is gone; its streams ended with it");
            }
        }
        if let Some(session) = attempt.session
            && !attempt.gone
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
        attempt: &mut Attempt,
    ) -> Event {
        let mut streams = Streams::default();

        // Await `$future` guarded by close/restart and the portal-side events, leaving the
        // attempt with the right event if one of those comes first or the call fails.
        macro_rules! step {
            ($future:expr) => {
                match self.watch(&mut streams, $future).await {
                    Err(stop) => {
                        attempt.gone = stop.session_gone();
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
            crate::portal::register_on(&connection).await;
        }
        let proxy = step!(Screencast::with_connection(connection));
        // Watch the portal's name before creating anything on it.
        streams.owner = Some(Box::pin(step!(proxy.receive_owner_changed())));

        attempt.session = Some(step!(proxy.create_session(Default::default())));
        let Some(session) = attempt.session.as_ref() else {
            return Event::Failed(Failure::Unavailable);
        };
        // Subscribe before `Start`: the dialog can end the session.
        streams.closed = Some(Box::pin(step!(session.receive_closed())));

        let options = SelectSourcesOptions::default()
            .set_sources(BitFlags::from_flag(SourceType::Monitor))
            .set_multiple(true)
            .set_cursor_mode(CursorMode::Hidden)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(restore_token.as_deref());
        let selected = step!(proxy.select_sources(session, options));
        if let Err(error) = selected.response() {
            return self.failed(&error);
        }

        tracing::info!("asking the ScreenCast portal to start the session");
        let started = step!(proxy.start(session, None, StartCastOptions::default()));
        let granted = match started.response() {
            Ok(granted) => granted,
            Err(error) => return self.failed(&error),
        };
        if granted.streams().is_empty() {
            tracing::info!("the user shared no monitor");
            return Event::Failed(Failure::Denied);
        }
        let portal_streams: Arc<[PortalStream]> = granted
            .streams()
            .iter()
            .filter_map(|stream| {
                let converted = PortalStream::from_portal(
                    stream.pipe_wire_node_id(),
                    stream.position(),
                    stream.size(),
                );
                if converted.is_none() {
                    tracing::warn!(
                        node = stream.pipe_wire_node_id(),
                        "ignoring a ScreenCast stream without a position and size"
                    );
                }
                converted
            })
            .collect();
        if portal_streams.is_empty() {
            tracing::warn!("the desktop granted no monitor stream with a position and size");
            return Event::Failed(Failure::Unavailable);
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

        let fd: OwnedFd = step!(proxy.open_pipe_wire_remote(session, Default::default()));

        let epoch = lifecycle.next_epoch();
        if self.commands.send(Command::Attach { epoch, fd }).is_err() {
            tracing::warn!("the PipeWire thread is gone");
            return Event::Failed(Failure::Unavailable);
        }
        attempt.attached = Some(epoch);
        if !self
            .shared
            .publish_active(epoch, portal_streams, &self.seen)
        {
            // A close or restart arrived while the session was starting: it is stale.
            return self
                .shared
                .take_change(&self.seen)
                .unwrap_or(Event::Restart);
        }
        for status in lifecycle.step(Event::Granted) {
            self.report(status);
        }
        tracing::info!(
            epoch,
            monitors = streams_len(&self.shared),
            "the ScreenCast session is active"
        );

        // The epoch lasts until the session closes, the portal or PipeWire goes away, or the
        // handle asks.
        match self.watch(&mut streams, std::future::pending::<()>()).await {
            Err(stop) => {
                attempt.gone = stop.session_gone();
                match stop {
                    Stop::SessionClosed => tracing::info!(
                        epoch,
                        "the portal closed the ScreenCast session (sharing was stopped)"
                    ),
                    Stop::PortalLost => {
                        tracing::warn!(epoch, "the ScreenCast portal went away")
                    }
                    Stop::CoreLost => {
                        tracing::warn!(epoch, "the PipeWire connection of the session was lost")
                    }
                    Stop::Event(_) => tracing::info!(epoch, "the ScreenCast session ended"),
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
            Failure::Denied => tracing::info!(%error, "the ScreenCast request was not granted"),
            Failure::Unavailable => {
                tracing::warn!(%error, "the ScreenCast portal is not usable")
            }
        }
        Event::Failed(failure)
    }
}

/// How many monitor streams the active epoch has (for the log).
fn streams_len(shared: &Shared) -> usize {
    shared.lock().active.as_ref().map_or(0, |a| a.streams.len())
}

/// Close the portal session, bounded. Errors only matter for the log.
async fn close_session(session: &Session<Screencast>) {
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

    fn streams(nodes: &[u32]) -> Arc<[PortalStream]> {
        nodes
            .iter()
            .map(|&node_id| PortalStream {
                node_id,
                position: (0, 0),
                size: (1920, 1080),
            })
            .collect()
    }

    /// A shared state as the worker leaves it after an epoch starts.
    fn activated(shared: &Shared, epoch: u64, seen: &Cell<u64>) {
        assert!(shared.publish_active(epoch, streams(&[7]), seen));
    }

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
            zbus::names::OwnedInterfaceName::try_from("org.freedesktop.portal.ScreenCast").unwrap();
        for error in [
            ashpd::Error::RequiresVersion(4, 1),
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
        // going away, or PipeWire dying, is a lost portal (retried once).
        assert_eq!(Stop::SessionClosed.into_event(true), Event::Revoked);
        assert_eq!(Stop::PortalLost.into_event(true), Event::PortalLost);
        assert_eq!(Stop::CoreLost.into_event(true), Event::PortalLost);
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
        assert!(!Stop::CoreLost.session_gone());
        assert!(!Stop::Event(Event::Close).session_gone());
    }

    #[test]
    fn epochs_end_their_streams_with_the_right_reason() {
        assert_eq!(detach_reason(Event::Revoked), StreamEndReason::Blocked);
        assert_eq!(detach_reason(Event::Close), StreamEndReason::Requested);
        assert_eq!(detach_reason(Event::PortalLost), StreamEndReason::Failed);
        assert_eq!(detach_reason(Event::Restart), StreamEndReason::Failed);
        assert_eq!(
            detach_reason(Event::Failed(Failure::Denied)),
            StreamEndReason::Failed
        );
    }

    #[test]
    fn handles_see_why_there_is_no_session() {
        let shared = Shared::new();
        let seen = Cell::new(0);
        assert!(matches!(
            shared.active(),
            Err(PlatformError::InteractionRequired)
        ));
        shared.lock().status = SessionStatus::Denied;
        assert!(matches!(
            shared.active(),
            Err(PlatformError::PermissionDenied(Permission::ScreenRecording))
        ));
        shared.lock().status = SessionStatus::Unavailable;
        assert!(matches!(
            shared.active(),
            Err(PlatformError::Unsupported(_))
        ));
        shared.lock().status = SessionStatus::Pending;
        activated(&shared, 1, &seen);
        let active = shared.active().unwrap();
        assert_eq!(active.epoch, 1);
        assert_eq!(active.streams.len(), 1);
        // The epoch ends: nothing to start any more.
        shared.end_epoch();
        shared.lock().status = SessionStatus::Closed { epoch: 1 };
        assert!(matches!(
            shared.active(),
            Err(PlatformError::InteractionRequired)
        ));
    }

    #[test]
    fn restart_and_close_drop_the_epoch_at_once() {
        let shared = Shared::new();
        let seen = Cell::new(0);
        activated(&shared, 1, &seen);
        shared.restart().unwrap();
        assert!(matches!(
            shared.active(),
            Err(PlatformError::InteractionRequired)
        ));
        // The worker sees one restart, then nothing.
        assert_eq!(
            Shared::change_locked(&shared.lock(), &seen),
            Some(Stop::Event(Event::Restart))
        );
        assert_eq!(Shared::change_locked(&shared.lock(), &seen), None);
        // A result of the old attempt arriving now is stale only if another change came since.
        activated(&shared, 2, &seen);
        shared.restart().unwrap();
        assert!(!shared.publish_active(3, streams(&[8]), &seen));
        shared.close();
        shared.close();
        assert!(matches!(shared.active(), Err(PlatformError::Backend(_))));
        assert!(shared.restart().is_err());
        assert_eq!(
            Shared::change_locked(&shared.lock(), &seen),
            Some(Stop::Event(Event::Close))
        );
        assert!(!shared.publish_active(4, streams(&[9]), &seen));
        assert_eq!(
            shared.lock().status,
            SessionStatus::Closed { epoch: 2 },
            "an active epoch closes with its own number"
        );
    }

    #[test]
    fn a_lost_pipewire_connection_ends_only_its_own_epoch() {
        let shared = Shared::new();
        let seen = Cell::new(0);
        // Reported before the epoch is published: it counts once the epoch is.
        shared.core_lost(1);
        assert_eq!(Shared::change_locked(&shared.lock(), &seen), None);
        activated(&shared, 1, &seen);
        assert!(matches!(shared.active(), Err(PlatformError::Backend(_))));
        assert_eq!(
            Shared::change_locked(&shared.lock(), &seen),
            Some(Stop::CoreLost)
        );
        assert_eq!(shared.take_change(&seen), Some(Event::PortalLost));
        // A report for an older epoch never touches a newer one.
        shared.end_epoch();
        shared.core_lost(1);
        activated(&shared, 2, &seen);
        assert!(shared.active().is_ok());
        assert_eq!(Shared::change_locked(&shared.lock(), &seen), None);
    }
}
