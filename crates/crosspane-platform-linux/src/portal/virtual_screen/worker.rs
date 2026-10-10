//! The VIRTUAL ScreenCast session worker and the consent-only run (WP-G2.4, task A).
//!
//! One worker thread owns one portal session through `ashpd`, driven with `zbus::block_on` on that
//! thread only (the style of `screencast::worker`). Unlike the monitor capture's worker it lives
//! exactly once: no restarts, no silent retries, no epochs. In order it
//!
//! 1. reads the stored restore token (no token: nothing is sent to the portal at all);
//! 2. connects, checks that the portal offers `SourceType::Virtual`, creates a session and selects
//!    one virtual source (`types = 4`, `multiple = false`, `CursorMode::Hidden`,
//!    `PersistMode::ExplicitlyRevoked`, the stored token);
//! 3. starts it. With a valid token this answers at once (about 10 ms live). A start that has not
//!    answered within `Timeouts::start` means the token is no longer good and a dialog is up: the
//!    session is closed (which dismisses the dialog), the token file is removed, and the screen is
//!    never created (`Unsupported`). `open` never waits on a dialog;
//! 4. stores the rotated token (0600, temp file + rename), calls `OpenPipeWireRemote` and hands the
//!    fd and the stream's node to the PipeWire thread (`Command::Attach`, `Command::Open`);
//! 5. watches the session's `Closed` signal, the portal's bus name and the PipeWire thread until
//!    the screen is over, then tells the PipeWire thread (`Command::Detach`) and closes the portal
//!    session, which is what removes Mutter's virtual monitor.
//!
//! **Endings.** `Closed` (the handle dropped or asked) ends the captures `Requested` and does not
//! call `on_lost`. The portal's `Closed` signal (the user stopped the share from the top bar), the
//! portal losing its bus name, or the PipeWire thread failing is a loss: the captures end
//! `TargetGone` and `on_lost` runs once, on this thread, before the session close call.
//!
//! **Consent.** [`consent`] is the consent-only run for agent start: a session that is started
//! without a token (the desktop shows its dialog), whose rotated token is stored, and that is
//! closed without ever opening a PipeWire remote.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::mpsc::{self, Sender};
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
use crosspane_platform::{PlatformError, StreamEndReason};
use crosspane_types::geom::PixelSize;
use zbus::export::futures_core::Stream;
use zbus::names::UniqueName;
use zbus::zvariant::OwnedValue;

use super::sleep::timeout;
use super::stream::Command;
use crate::portal::screencast::token;

/// How long all portal calls before `Start` may take together (connect, version checks,
/// `CreateSession`, `SelectSources`).
const SETUP_TIMEOUT: Duration = Duration::from_secs(3);
/// How long `Start` may take with a stored token before it is taken for a dialog.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `OpenPipeWireRemote` may take.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the first format negotiation may take, after the stream was opened.
const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long closing a portal session may take before the worker gives up on the reply.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long dropping the handle waits for the worker before detaching it.
const JOIN_TIMEOUT: Duration = Duration::from_secs(3);
/// Slack on top of a consent run's own bounds, for the caller's wait.
const CONSENT_SLACK: Duration = Duration::from_secs(5);

/// The bounds a screen is opened under. Tests shorten them.
#[derive(Clone, Copy, Debug)]
pub(super) struct Timeouts {
    pub setup: Duration,
    pub start: Duration,
    pub remote: Duration,
    pub negotiate: Duration,
}

impl Default for Timeouts {
    fn default() -> Timeouts {
        Timeouts {
            setup: SETUP_TIMEOUT,
            start: START_TIMEOUT,
            remote: REMOTE_TIMEOUT,
            negotiate: NEGOTIATE_TIMEOUT,
        }
    }
}

impl Timeouts {
    /// The longest the portal side of `open` can take: the worker bounds each step, this is the
    /// handle's belt and braces.
    fn portal_guard(&self) -> Duration {
        self.setup + self.start + self.remote + Duration::from_secs(2)
    }
}

// ---- state shared with the handle and the PipeWire thread --------------------------------------

/// How far the screen has come.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Stage {
    /// The portal session is being set up.
    #[default]
    Starting,
    /// The PipeWire stream was handed over; the first format is not negotiated yet.
    Connecting,
}

#[derive(Default)]
struct State {
    stage: Stage,
    /// The size the stream is negotiated at now; `None` while there is none (not yet, or a
    /// renegotiation).
    negotiated: Option<PixelSize>,
    /// The last size the stream negotiated (the asked-for size until the first).
    size: PixelSize,
    /// Why the screen failed or was lost, for the call that is waiting.
    error: Option<PlatformError>,
    /// `open` succeeded.
    ready: bool,
    /// The screen is over, whatever the cause.
    over: bool,
    /// Over because it was lost (not because the handle closed it).
    lost: bool,
    /// The handle asked the worker to stop.
    closing: bool,
    /// The PipeWire thread failed.
    pw_lost: bool,
    on_lost: Option<Arc<dyn Fn() + Send + Sync>>,
    exited: bool,
    waker: Option<Waker>,
}

/// What the handle, the worker and the PipeWire thread share.
pub(super) struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    pub(super) fn new(size: PixelSize) -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(State {
                size,
                ..State::default()
            }),
            changed: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The screen exists and is not over.
    pub(super) fn is_live(&self) -> bool {
        let state = self.lock();
        state.ready && !state.over
    }

    /// The last negotiated size.
    pub(super) fn size(&self) -> PixelSize {
        self.lock().size
    }

    /// Set by the worker once the portal side is done.
    fn set_stage(&self, stage: Stage) {
        self.lock().stage = stage;
        self.changed.notify_all();
    }

    /// Set by the PipeWire thread whenever the stream's format changes.
    pub(super) fn set_negotiated(&self, size: Option<PixelSize>) {
        {
            let mut state = self.lock();
            state.negotiated = size;
            if let Some(size) = size {
                state.size = size;
            }
        }
        self.changed.notify_all();
    }

    /// The PipeWire thread cannot go on (connection lost, stream failed or gone, or it could not
    /// be set up). Wakes the worker, which ends the screen as lost.
    pub(super) fn pw_failed(&self, error: PlatformError) {
        let waker = {
            let mut state = self.lock();
            state.error.get_or_insert(error);
            state.pw_lost = true;
            state.waker.take()
        };
        self.changed.notify_all();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// The handle is done with the screen: the worker closes the session and exits. No `on_lost`.
    pub(super) fn request_close(&self) {
        let waker = {
            let mut state = self.lock();
            state.closing = true;
            state.on_lost = None;
            state.waker.take()
        };
        self.changed.notify_all();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Register the loss callback. A screen that is already lost calls it at once, here.
    pub(super) fn set_on_lost(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        let run_now = {
            let mut state = self.lock();
            if state.lost {
                Some(callback)
            } else {
                if !state.closing {
                    state.on_lost = Some(callback);
                }
                None
            }
        };
        if let Some(callback) = run_now {
            call(&callback);
        }
    }

    /// What the worker should do next, if anything; otherwise remember `waker` for the next change.
    fn poll_stop(&self, waker: &Waker) -> Option<Stop> {
        let mut state = self.lock();
        if state.closing {
            return Some(Stop::Close);
        }
        if state.pw_lost {
            return Some(Stop::PwLost);
        }
        if !matches!(&state.waker, Some(old) if old.will_wake(waker)) {
            state.waker = Some(waker.clone());
        }
        None
    }

    /// The screen is over. Idempotent. A loss runs the `on_lost` callback (outside the lock).
    fn finish(&self, end: End) {
        let callback = {
            let mut state = self.lock();
            if state.over {
                return;
            }
            state.over = true;
            match end {
                End::Closed => None,
                End::Lost => {
                    state.lost = true;
                    state.on_lost.take()
                }
                End::Failed(error) => {
                    state.error.get_or_insert(error);
                    None
                }
            }
        };
        self.changed.notify_all();
        if let Some(callback) = callback {
            call(&callback);
        }
    }

    /// Wait until `check` returns something, or `limit` passes (`None`).
    fn wait_for<T>(
        &self,
        limit: Duration,
        mut check: impl FnMut(&mut State) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + limit;
        let mut state = self.lock();
        loop {
            if let Some(value) = check(&mut state) {
                return Some(value);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// The handle's wait in `open`: the portal side first, then the format.
    pub(super) fn await_ready(
        &self,
        size: PixelSize,
        timeouts: &Timeouts,
    ) -> Result<(), PlatformError> {
        let portal = self.wait_for(timeouts.portal_guard(), |state| {
            if let Some(error) = state.error.take() {
                return Some(Err(error));
            }
            if state.over {
                return Some(Err(gone()));
            }
            (state.stage != Stage::Starting).then_some(Ok(()))
        });
        match portal {
            None => return Err(PlatformError::Timeout),
            Some(Err(error)) => return Err(error),
            Some(Ok(())) => {}
        }
        let negotiated = self.wait_for(timeouts.negotiate, |state| {
            if let Some(error) = state.error.take() {
                return Some(Err(error));
            }
            if state.over {
                return Some(Err(gone()));
            }
            if state.negotiated == Some(size) {
                state.ready = true;
                return Some(Ok(()));
            }
            None
        });
        negotiated.unwrap_or(Err(PlatformError::Timeout))
    }

    /// The handle's wait in `resize`: until the stream runs at `size`.
    pub(super) fn await_size(&self, size: PixelSize, limit: Duration) -> Result<(), PlatformError> {
        self.wait_for(limit, |state| {
            if let Some(error) = state.error.take() {
                return Some(Err(error));
            }
            if state.over {
                return Some(Err(gone()));
            }
            (state.negotiated == Some(size)).then_some(Ok(()))
        })
        .unwrap_or(Err(PlatformError::Timeout))
    }

    /// [`request_close`](Self::request_close), then wait (bounded) for the worker to exit.
    pub(super) fn close_and_join(&self, handle: JoinHandle<()>) {
        self.request_close();
        // A loss callback that drops the screen runs on the worker itself.
        if handle.thread().id() == thread::current().id() {
            return;
        }
        let exited = self
            .wait_for(JOIN_TIMEOUT, |state| state.exited.then_some(()))
            .is_some();
        if exited {
            if handle.join().is_err() {
                tracing::warn!("the virtual screen worker panicked");
            }
        } else {
            tracing::warn!("the virtual screen worker did not stop in time; detaching it");
        }
    }
}

fn gone() -> PlatformError {
    PlatformError::NotFound
}

/// Run a callback that must not take the thread down.
fn call(callback: &Arc<dyn Fn() + Send + Sync>) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback())).is_err() {
        tracing::warn!("a virtual screen loss callback panicked");
    }
}

/// Marks the worker as exited when it ends, even by a panic, and ends the screen if the worker
/// did not.
struct ExitGuard(Arc<Shared>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0.finish(End::Lost);
        self.0.lock().exited = true;
        self.0.changed.notify_all();
    }
}

// ---- the worker --------------------------------------------------------------------------------

/// Why a guarded step stopped early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    /// The handle dropped the screen.
    Close,
    /// The portal emitted `Closed` for our session.
    SessionClosed,
    /// The portal's bus name changed owner or vanished.
    PortalLost,
    /// The PipeWire thread failed.
    PwLost,
}

impl Stop {
    /// The session no longer exists on the portal's side.
    fn session_gone(self) -> bool {
        matches!(self, Stop::SessionClosed | Stop::PortalLost)
    }
}

/// How the screen ended.
#[derive(Debug)]
enum End {
    /// The handle closed it.
    Closed,
    /// It was running (or starting its stream) and went away.
    Lost,
    /// It never got going.
    Failed(PlatformError),
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
    /// The PipeWire thread was told to open the stream.
    attached: bool,
}

/// Everything a screen's worker needs.
pub(super) struct Job {
    pub shared: Arc<Shared>,
    /// To the PipeWire thread.
    pub commands: Sender<Command>,
    pub token_path: PathBuf,
    /// `None`: the session bus.
    pub bus_address: Option<String>,
    pub size: PixelSize,
    pub timeouts: Timeouts,
}

/// Start the worker thread. It runs until the screen is over.
pub(super) fn spawn(job: Job) -> Result<JoinHandle<()>, PlatformError> {
    thread::Builder::new()
        .name("crosspane-portal-virtual".to_owned())
        .spawn(move || {
            let _exit = ExitGuard(Arc::clone(&job.shared));
            zbus::block_on(job.drive());
        })
        .map_err(|error| {
            PlatformError::Backend(format!("cannot start the virtual screen worker: {error}"))
        })
}

fn select_options(restore_token: Option<&str>) -> SelectSourcesOptions {
    SelectSourcesOptions::default()
        .set_sources(BitFlags::from_flag(SourceType::Virtual))
        .set_multiple(false)
        .set_cursor_mode(CursorMode::Hidden)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(restore_token)
}

async fn connect(bus_address: Option<&str>) -> Result<zbus::Connection, zbus::Error> {
    match bus_address {
        None => zbus::Connection::session().await,
        Some(address) => zbus::connection::Builder::address(address)?.build().await,
    }
}

/// A user's (or the desktop's) "no" to a request.
fn is_denial(error: &ashpd::Error) -> bool {
    matches!(
        error,
        ashpd::Error::Response(_)
            | ashpd::Error::Portal(PortalError::Cancelled(_) | PortalError::NotAllowed(_))
    )
}

fn map_error(error: &ashpd::Error) -> PlatformError {
    match error {
        _ if is_denial(error) => {
            PlatformError::Unsupported("the desktop did not grant the virtual screen")
        }
        ashpd::Error::PortalNotFound(_) | ashpd::Error::RequiresVersion(..) => {
            PlatformError::Unsupported("no ScreenCast portal with virtual sources")
        }
        ashpd::Error::Zbus(zbus::Error::FDO(inner))
            if matches!(**inner, zbus::fdo::Error::ServiceUnknown(_)) =>
        {
            PlatformError::Unsupported("no ScreenCast portal")
        }
        other => PlatformError::Backend(format!("virtual screen: {other}")),
    }
}

/// Close the portal session, bounded. Errors only matter for the log.
async fn close_session(session: &Session<Screencast>) {
    match timeout(session.close(), CLOSE_TIMEOUT).await {
        Some(Ok(())) => {}
        Some(Err(error)) => tracing::debug!(%error, "closing the portal session failed"),
        None => tracing::warn!("closing the portal session timed out"),
    }
}

impl Job {
    async fn drive(&self) {
        let mut attempt = Attempt::default();
        let end = self.attempt(&mut attempt).await;
        let reason = match end {
            End::Closed => StreamEndReason::Requested,
            End::Lost | End::Failed(_) => StreamEndReason::TargetGone,
        };
        if attempt.attached && self.commands.send(Command::Detach { reason }).is_err() {
            tracing::warn!("the PipeWire thread is gone; its captures ended with it");
        }
        // Before the close call: a loss is acted on at once.
        self.shared.finish(end);
        if let Some(session) = attempt.session.take()
            && !attempt.gone
        {
            // Closing also dismisses a consent dialog that is still up, and removes the virtual
            // monitor. A session the portal already closed, or a portal that is gone, needs no
            // call (it could even restart the portal).
            close_session(&session).await;
        }
    }

    /// Poll `future` unless the handle closes the screen, the PipeWire thread fails, or a
    /// portal-side event comes first, and in that case drop it. A ready result comes before a
    /// portal-side `Closed`, so a cancelled dialog is still its response and not its session's
    /// closing.
    async fn watch<F: Future>(
        &self,
        streams: &mut Streams<'_>,
        future: F,
    ) -> Result<F::Output, Stop> {
        let mut future = std::pin::pin!(future);
        poll_fn(|cx| {
            if let Some(stop) = self.shared.poll_stop(cx.waker()) {
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

    /// How an attempt ends when `stop` interrupts it.
    fn stopped(&self, stop: Stop, attempt: &Attempt) -> End {
        match stop {
            Stop::Close => End::Closed,
            Stop::SessionClosed | Stop::PortalLost | Stop::PwLost if attempt.attached => {
                match stop {
                    Stop::SessionClosed => tracing::info!(
                        "the portal closed the virtual screen session (sharing was stopped)"
                    ),
                    Stop::PortalLost => tracing::warn!("the ScreenCast portal went away"),
                    _ => tracing::warn!("the PipeWire side of the virtual screen failed"),
                }
                End::Lost
            }
            Stop::SessionClosed => End::Failed(PlatformError::Unsupported(
                "the desktop closed the virtual screen request",
            )),
            Stop::PortalLost | Stop::PwLost => End::Failed(PlatformError::Unsupported(
                "the ScreenCast portal went away while starting",
            )),
        }
    }

    /// One start attempt and, if it succeeds, the screen's life until it ends.
    async fn attempt(&self, attempt: &mut Attempt) -> End {
        let mut streams = Streams::default();
        let timeouts = self.timeouts;
        let setup_until = Instant::now() + timeouts.setup;
        let left = || setup_until.saturating_duration_since(Instant::now());

        // Await `$future` for at most `$limit`, guarded by the handle and the portal-side events,
        // leaving the attempt with the right end if one of those comes first, the time runs out
        // or the call fails.
        macro_rules! step {
            ($limit:expr, $future:expr) => {
                match self.watch(&mut streams, timeout($future, $limit)).await {
                    Err(stop) => {
                        attempt.gone = stop.session_gone();
                        return self.stopped(stop, attempt);
                    }
                    Ok(None) => return End::Failed(PlatformError::Timeout),
                    Ok(Some(Err(error))) => {
                        let error: ashpd::Error = error.into();
                        tracing::warn!(%error, "the ScreenCast portal is not usable");
                        return End::Failed(map_error(&error));
                    }
                    Ok(Some(Ok(value))) => value,
                }
            };
        }

        // A token is the consent. Without one nothing is asked of the portal (a dialog would be
        // the user's, and `open` never waits for it).
        let Some(restore_token) = token::read(&self.token_path) else {
            return End::Failed(PlatformError::Unsupported(
                "no stored consent for a virtual screen",
            ));
        };

        let connection = step!(left(), connect(self.bus_address.as_deref()));
        if self.bus_address.is_none() {
            let _ = timeout(crate::portal::register_on(&connection), left()).await;
        }
        let proxy = step!(left(), Screencast::with_connection(connection));
        // Watch the portal's name before creating anything on it.
        streams.owner = Some(Box::pin(step!(left(), proxy.receive_owner_changed())));
        let offered = step!(left(), proxy.available_source_types());
        if !offered.contains(SourceType::Virtual) {
            return End::Failed(PlatformError::Unsupported(
                "the ScreenCast portal offers no virtual source",
            ));
        }

        attempt.session = Some(step!(left(), proxy.create_session(Default::default())));
        let Some(session) = attempt.session.as_ref() else {
            return End::Failed(PlatformError::Backend("virtual screen: no session".into()));
        };
        // Subscribe before `Start`: the dialog can end the session.
        streams.closed = Some(Box::pin(step!(left(), session.receive_closed())));

        let selected = step!(
            left(),
            proxy.select_sources(session, select_options(Some(&restore_token)))
        );
        if let Err(error) = selected.response() {
            return self.denied_or_failed(&error);
        }

        tracing::info!("asking the ScreenCast portal for the stored virtual screen");
        let started = match self
            .watch(
                &mut streams,
                timeout(
                    proxy.start(session, None, StartCastOptions::default()),
                    timeouts.start,
                ),
            )
            .await
        {
            Err(stop) => {
                attempt.gone = stop.session_gone();
                if stop == Stop::SessionClosed {
                    // The dialog (or the desktop) ended the request: the grant is gone.
                    token::remove(&self.token_path);
                }
                return self.stopped(stop, attempt);
            }
            Ok(None) => {
                tracing::warn!(
                    "the portal did not answer the stored consent in time (a dialog may be up); \
                     dismissing it and forgetting the token"
                );
                token::remove(&self.token_path);
                return End::Failed(PlatformError::Unsupported(
                    "the desktop asks for consent again for the virtual screen",
                ));
            }
            Ok(Some(Err(error))) => {
                let error: ashpd::Error = error;
                tracing::warn!(%error, "the ScreenCast portal is not usable");
                return End::Failed(map_error(&error));
            }
            Ok(Some(Ok(request))) => request,
        };
        let granted = match started.response() {
            Ok(granted) => granted,
            Err(error) => return self.denied_or_failed(&error),
        };
        let Some(stream) = granted.streams().first() else {
            tracing::info!("the desktop shared no virtual screen");
            return End::Failed(PlatformError::Unsupported(
                "the desktop shared no virtual screen",
            ));
        };
        if granted.streams().len() > 1 {
            tracing::warn!(
                streams = granted.streams().len(),
                "more than one stream for one virtual source; using the first"
            );
        }
        if stream
            .source_type()
            .is_some_and(|t| t != SourceType::Virtual)
        {
            // Fail closed: never fixate the size of a stream that is a real monitor or window.
            return End::Failed(PlatformError::Unsupported(
                "the desktop shared something other than a virtual screen",
            ));
        }
        let node_id = stream.pipe_wire_node_id();
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

        let fd: OwnedFd = step!(
            timeouts.remote,
            proxy.open_pipe_wire_remote(session, Default::default())
        );

        if self.commands.send(Command::Attach { fd }).is_err()
            || self
                .commands
                .send(Command::Open {
                    node_id,
                    size: self.size,
                })
                .is_err()
        {
            tracing::warn!("the PipeWire thread is gone");
            return End::Failed(PlatformError::Backend(
                "virtual screen: the PipeWire thread is gone".into(),
            ));
        }
        attempt.attached = true;
        self.shared.set_stage(Stage::Connecting);
        tracing::info!(
            width = self.size.width,
            height = self.size.height,
            "the virtual screen session is started"
        );

        // The screen lasts until the handle closes it, the session is closed from outside, the
        // portal goes away or PipeWire fails.
        match self.watch(&mut streams, std::future::pending::<()>()).await {
            Err(stop) => {
                attempt.gone = stop.session_gone();
                self.stopped(stop, attempt)
            }
            // Unreachable (the future never completes); never carry on.
            Ok(()) => End::Lost,
        }
    }

    /// A refused request forgets the token (the grant is gone); anything else is a failure.
    fn denied_or_failed(&self, error: &ashpd::Error) -> End {
        if is_denial(error) {
            tracing::info!(%error, "the virtual screen request was not granted");
            token::remove(&self.token_path);
        } else {
            tracing::warn!(%error, "the ScreenCast portal is not usable");
        }
        End::Failed(map_error(error))
    }
}

// ---- the consent-only run ----------------------------------------------------------------------

/// Bound a portal call by `limit`, mapping its error.
async fn bounded<T, E: Into<ashpd::Error>>(
    limit: Duration,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, PlatformError> {
    match timeout(future, limit).await {
        None => Err(PlatformError::Timeout),
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(map_error(&error.into())),
    }
}

/// Consent-only run: see [`super::prepare_consent`]. Runs the portal conversation on its own
/// thread and waits for it at most `wait` plus a little.
pub(super) fn consent(
    token_path: PathBuf,
    wait: Duration,
    bus_address: Option<String>,
) -> Result<bool, PlatformError> {
    if token::read(&token_path).is_some() {
        return Ok(true);
    }
    let (result, answer) = mpsc::channel();
    thread::Builder::new()
        .name("crosspane-portal-consent".to_owned())
        .spawn(move || {
            let _ = result.send(zbus::block_on(run_consent(&token_path, wait, bus_address)));
        })
        .map_err(|error| {
            PlatformError::Backend(format!("cannot start the consent worker: {error}"))
        })?;
    match answer.recv_timeout(wait + SETUP_TIMEOUT + CLOSE_TIMEOUT + CONSENT_SLACK) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(PlatformError::Timeout),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(PlatformError::Backend(
            "virtual screen: the consent worker stopped".into(),
        )),
    }
}

async fn run_consent(
    token_path: &Path,
    wait: Duration,
    bus_address: Option<String>,
) -> Result<bool, PlatformError> {
    let setup_until = Instant::now() + SETUP_TIMEOUT;
    let left = || setup_until.saturating_duration_since(Instant::now());
    let connection = bounded(left(), connect(bus_address.as_deref())).await?;
    if bus_address.is_none() {
        let _ = timeout(crate::portal::register_on(&connection), left()).await;
    }
    let proxy = bounded(left(), Screencast::with_connection(connection)).await?;
    let offered = bounded(left(), proxy.available_source_types()).await?;
    if !offered.contains(SourceType::Virtual) {
        return Err(PlatformError::Unsupported(
            "the ScreenCast portal offers no virtual source",
        ));
    }
    let session = bounded(left(), proxy.create_session(Default::default())).await?;
    let result = consent_session(&proxy, &session, token_path, wait, left()).await;
    // Whatever happened, the session ends here: it also dismisses a dialog that is still up, and
    // it never had a PipeWire consumer, so no monitor was created.
    close_session(&session).await;
    result
}

async fn consent_session(
    proxy: &Screencast,
    session: &Session<Screencast>,
    token_path: &Path,
    wait: Duration,
    setup_left: Duration,
) -> Result<bool, PlatformError> {
    let deadline = Instant::now() + wait;
    let selected = bounded(
        setup_left,
        proxy.select_sources(session, select_options(None)),
    )
    .await?;
    if let Err(error) = selected.response() {
        return if is_denial(&error) {
            Ok(false)
        } else {
            Err(map_error(&error))
        };
    }
    tracing::info!("asking the ScreenCast portal for consent to a virtual screen");
    let remaining = deadline.saturating_duration_since(Instant::now());
    let request = match timeout(
        proxy.start(session, None, StartCastOptions::default()),
        remaining,
    )
    .await
    {
        None => {
            tracing::info!("no answer to the virtual screen consent in time; dismissing it");
            return Ok(false);
        }
        Some(Err(error)) => return Err(map_error(&error)),
        Some(Ok(request)) => request,
    };
    match request.response() {
        Ok(granted) => match granted.restore_token() {
            Some(rotated) => match token::write(token_path, rotated) {
                Ok(()) => Ok(true),
                Err(error) => {
                    tracing::warn!(%error, "cannot store the portal restore token");
                    Ok(false)
                }
            },
            None => {
                tracing::warn!("the portal returned no restore token; consent cannot be kept");
                Ok(false)
            }
        },
        Err(error) if is_denial(&error) => {
            tracing::info!(%error, "the virtual screen consent was not given");
            Ok(false)
        }
        Err(error) => Err(map_error(&error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ashpd::desktop::ResponseError;

    #[test]
    fn refusals_are_denials_and_other_failures_are_not() {
        for error in [
            ashpd::Error::Response(ResponseError::Cancelled),
            ashpd::Error::Response(ResponseError::Other),
            ashpd::Error::Portal(PortalError::Cancelled(String::new())),
            ashpd::Error::Portal(PortalError::NotAllowed(String::new())),
        ] {
            assert!(is_denial(&error), "{error}");
            assert!(matches!(map_error(&error), PlatformError::Unsupported(_)));
        }
        for error in [
            ashpd::Error::NoResponse,
            ashpd::Error::Portal(PortalError::Failed(String::new())),
            ashpd::Error::Zbus(zbus::Error::Failure(String::new())),
        ] {
            assert!(!is_denial(&error), "{error}");
            assert!(matches!(map_error(&error), PlatformError::Backend(_)));
        }
        let missing =
            zbus::names::OwnedInterfaceName::try_from("org.freedesktop.portal.ScreenCast").unwrap();
        for error in [
            ashpd::Error::PortalNotFound(missing),
            ashpd::Error::RequiresVersion(4, 1),
            ashpd::Error::Zbus(zbus::Error::FDO(Box::new(
                zbus::fdo::Error::ServiceUnknown(String::new()),
            ))),
        ] {
            assert!(
                matches!(map_error(&error), PlatformError::Unsupported(_)),
                "{error}"
            );
        }
    }

    #[test]
    fn the_handle_sees_the_negotiated_size_and_a_failure() {
        let size = PixelSize::new(800, 600);
        let shared = Shared::new(size);
        assert!(!shared.is_live());
        let timeouts = Timeouts {
            setup: Duration::from_millis(50),
            start: Duration::from_millis(50),
            remote: Duration::from_millis(50),
            negotiate: Duration::from_millis(60),
        };
        shared.set_stage(Stage::Connecting);
        // Connecting but never negotiated: Timeout.
        assert!(matches!(
            shared.await_ready(size, &timeouts),
            Err(PlatformError::Timeout)
        ));
        // Negotiated at another size does not count.
        shared.set_negotiated(Some(PixelSize::new(1, 1)));
        assert!(matches!(
            shared.await_ready(size, &timeouts),
            Err(PlatformError::Timeout)
        ));
        assert!(!shared.is_live());
        shared.set_negotiated(Some(size));
        shared.await_ready(size, &timeouts).unwrap();
        assert!(shared.is_live());
        assert_eq!(shared.size(), size);
        // A renegotiation: no size for a while, then the new one.
        shared.set_negotiated(None);
        assert_eq!(shared.size(), size, "the last size stays");
        assert!(matches!(
            shared.await_size(PixelSize::new(900, 700), Duration::from_millis(30)),
            Err(PlatformError::Timeout)
        ));
        shared.set_negotiated(Some(PixelSize::new(900, 700)));
        shared
            .await_size(PixelSize::new(900, 700), Duration::from_millis(30))
            .unwrap();
        assert_eq!(shared.size(), PixelSize::new(900, 700));
        // A failure reaches the waiter once.
        shared.pw_failed(PlatformError::Backend("boom".into()));
        assert!(matches!(
            shared.await_size(PixelSize::new(1000, 700), Duration::from_millis(30)),
            Err(PlatformError::Backend(_))
        ));
    }

    #[test]
    fn loss_runs_the_callback_once_and_close_never_does() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let count = Arc::new(AtomicU32::new(0));
        let callback = |count: &Arc<AtomicU32>| -> Arc<dyn Fn() + Send + Sync> {
            let count = Arc::clone(count);
            Arc::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
            })
        };

        let lost = Shared::new(PixelSize::new(1, 1));
        lost.set_on_lost(callback(&count));
        lost.finish(End::Lost);
        lost.finish(End::Lost);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        // Registered after the loss: runs at once.
        lost.set_on_lost(callback(&count));
        assert_eq!(count.load(Ordering::SeqCst), 2);

        let closed = Shared::new(PixelSize::new(1, 1));
        closed.set_on_lost(callback(&count));
        closed.request_close();
        closed.finish(End::Closed);
        closed.finish(End::Lost);
        assert_eq!(count.load(Ordering::SeqCst), 2, "a close is not a loss");
        // A callback that panics does not take the worker down.
        let boom = Shared::new(PixelSize::new(1, 1));
        boom.set_on_lost(Arc::new(|| panic!("expected in this test")));
        boom.finish(End::Lost);
    }

    #[test]
    fn stops_end_the_attempt_by_how_far_it_got() {
        let job = Job {
            shared: Shared::new(PixelSize::new(1, 1)),
            commands: mpsc::channel().0,
            token_path: PathBuf::from("unused"),
            bus_address: None,
            size: PixelSize::new(1, 1),
            timeouts: Timeouts::default(),
        };
        let early = Attempt::default();
        let running = Attempt {
            attached: true,
            ..Attempt::default()
        };
        assert!(matches!(job.stopped(Stop::Close, &early), End::Closed));
        assert!(matches!(job.stopped(Stop::Close, &running), End::Closed));
        for stop in [Stop::SessionClosed, Stop::PortalLost, Stop::PwLost] {
            assert!(matches!(job.stopped(stop, &running), End::Lost), "{stop:?}");
            assert!(
                matches!(
                    job.stopped(stop, &early),
                    End::Failed(PlatformError::Unsupported(_))
                ),
                "{stop:?}"
            );
        }
        assert!(Stop::SessionClosed.session_gone());
        assert!(Stop::PortalLost.session_gone());
        assert!(!Stop::PwLost.session_gone());
        assert!(!Stop::Close.session_gone());
    }
}
