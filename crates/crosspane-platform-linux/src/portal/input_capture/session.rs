//! The worker that owns the InputCapture portal session (`ashpd`, driven with `zbus::block_on` on
//! this thread only).
//!
//! **Lifecycle.** The session is created lazily, on the first non-empty portal set, and never inside
//! a trait call: the worker idles until `set_portals` wants something, then
//!
//! 1. connects to the session bus, registers the app id, opens the InputCapture portal and watches
//!    its bus name;
//! 2. creates the session. A portal of version 1 (GNOME 50) takes the legacy `CreateSession`,
//!    which shows a consent dialog on **every** call; a portal of version 2 or later takes
//!    `CreateSession2` and `Start` with persist mode 2 and the rotated restore token at
//!    `token_path`, so later starts are silent;
//! 3. subscribes to the portal's signals as **one** stream (the proxy's all-signals stream), so
//!    `Activated`, `Deactivated`, `ZonesChanged` and `Disabled` are handled in the order the
//!    portal sent them;
//! 4. installs the barriers (below) and serves signals, set changes and re-arm requests until the
//!    session ends.
//!
//! **Install.** GetZones, plan (`barriers`), `SetPointerBarriers`, and, once per session,
//! `ConnectToEIS` and the receiver's handshake and bind, then `Enable`. Mutter refuses `Enable`
//! before `ConnectToEIS` ("Not connected to EIS"), so the first install connects before enabling.
//! Replacing barriers is `Disable`, `SetPointerBarriers`, `Enable` (mutter wants the session in its
//! initial state to change barriers); the KDE quirk skips the `Disable` (xdp-kde's `Disable`
//! re-enables the session). A replacement is deferred while an activation is pending or active: the
//! compositor allows no new activation meanwhile, and the set is installed when it is over.
//!
//! **Why this thread never releases.** `Release` goes through the release thread (see the parent
//! module), which uses the same bus connection but never waits for this thread: this thread can be
//! stuck on a consent dialog for as long as the user takes.
//!
//! **Ends.** The user's "no" is `Denied` and a missing portal `Unavailable`; neither is retried
//! until the agent restarts. The session's `Closed` signal (the user stopped it in the desktop's
//! indicator) is `Closed` and stays so. The portal's bus name vanishing is retried once, after 1 s.
//! Logs never contain key or button codes; the restore token's value is never logged.

use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use ashpd::PortalError;
use ashpd::desktop::PersistMode;
use ashpd::desktop::input_capture::{
    Activated, ActivatedBarrier, Barrier, BarrierPosition, Capabilities, ConnectToEISOptions,
    CreateSession2Options, CreateSessionOptions, Deactivated, DisableOptions, EnableOptions,
    GetZonesOptions, InputCapture, SetPointerBarriersOptions, StartOptions,
};
use crosspane_platform::{CapturePortal, PlatformError};
use zbus::export::futures_core::Stream;

use super::barriers::{self, Zone};
use super::sleep::{Sleep, timeout};
use super::{ATTACH_BOUND, ApplyError, CaptureStatus, EisCmd, PortalHandle, Shared, token};

/// How long one portal call may take (none of them waits for the user).
const CALL_BOUND: Duration = Duration::from_secs(2);
/// How long closing the session may take.
const CLOSE_BOUND: Duration = Duration::from_millis(1500);
/// Wait before the single retry after the portal's bus name vanished.
const RETRY_DELAY: Duration = Duration::from_secs(1);
/// A session that ended within this long of starting, asking for a restart, is not restarted again:
/// something is wrong and every start is a consent dialog.
const MIN_LIFE: Duration = Duration::from_secs(5);
/// A failed install is retried this often, this many times.
const INSTALL_RETRY: Duration = Duration::from_secs(1);
const INSTALL_RETRIES: u32 = 3;

/// Why a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// The handle was dropped.
    Closing,
    /// Something asked for a fresh session (the EIS connection died).
    Restart,
    /// The quirk `close_to_replace` closed the session to replace or drop the barriers.
    Reset,
    Denied,
    Unavailable,
    /// The portal closed the session (the user stopped it).
    Closed,
    /// The portal's bus name changed owner or vanished.
    PortalLost,
}

/// Which signal or event the serve loop woke for.
enum Event {
    Control,
    Signal(Option<zbus::Message>),
    SessionClosed,
    OwnerChanged,
    Retry,
}

/// Per-session install state.
#[derive(Default)]
struct Install {
    /// `ConnectToEIS` and the receiver's attach are done (once per session).
    eis: bool,
    /// An `Enable` was sent in this session.
    enabled_once: bool,
}

pub(super) struct Worker {
    pub(super) shared: Arc<Shared>,
    /// Seen version of the control state.
    seen: Cell<u64>,
}

impl Worker {
    pub(super) fn new(shared: Arc<Shared>) -> Worker {
        Worker {
            shared,
            seen: Cell::new(0),
        }
    }

    pub(super) fn run(self) {
        let _exit = super::WorkerExit(Arc::clone(&self.shared));
        zbus::block_on(self.drive());
    }

    async fn drive(&self) {
        let mut portal_retries = 0u32;
        loop {
            if !self.wait_wanted().await {
                return;
            }
            let started = Instant::now();
            let end = self.session().await;
            tracing::info!(?end, "the input capture session ended");
            match end {
                End::Closing => return,
                End::Denied => {
                    self.shared.status.set(CaptureStatus::Denied);
                    self.park().await;
                    return;
                }
                End::Unavailable => {
                    self.shared.status.set(CaptureStatus::Unavailable);
                    self.park().await;
                    return;
                }
                End::Closed => {
                    self.shared.status.set(CaptureStatus::Closed);
                    self.park().await;
                    return;
                }
                End::PortalLost => {
                    if started.elapsed() >= Duration::from_secs(30) {
                        portal_retries = 0;
                    }
                    if portal_retries >= 1 {
                        self.shared.status.set(CaptureStatus::Closed);
                        self.park().await;
                        return;
                    }
                    portal_retries += 1;
                    self.shared.status.set(CaptureStatus::Closed);
                    if self.sleep(RETRY_DELAY).await {
                        return;
                    }
                }
                End::Restart => {
                    if started.elapsed() < MIN_LIFE {
                        self.shared.status.set(CaptureStatus::Unavailable);
                        self.park().await;
                        return;
                    }
                }
                End::Reset => {}
            }
            self.shared.status.set(CaptureStatus::Idle);
        }
    }

    /// Wait until `set_portals` wants a non-empty set. `false`: the handle was dropped.
    async fn wait_wanted(&self) -> bool {
        poll_fn(|cx| {
            let state = self.shared.control.poll(&self.seen, cx.waker());
            if state.closing {
                return Poll::Ready(false);
            }
            if state.wanted_len > 0 {
                return Poll::Ready(true);
            }
            Poll::Pending
        })
        .await
    }

    /// Idle until the handle is dropped (a terminal status).
    async fn park(&self) {
        poll_fn(|cx| {
            if self.shared.control.poll(&self.seen, cx.waker()).closing {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    /// Sleep, unless the handle is dropped first (`true`).
    async fn sleep(&self, duration: Duration) -> bool {
        let mut sleep = pin!(Sleep::new(duration));
        poll_fn(|cx| {
            if self.shared.control.poll(&self.seen, cx.waker()).closing {
                return Poll::Ready(true);
            }
            if sleep.as_mut().poll(cx).is_ready() {
                return Poll::Ready(false);
            }
            Poll::Pending
        })
        .await
    }

    async fn connect(&self) -> Result<zbus::Connection, zbus::Error> {
        match &self.shared.bus_address {
            None => zbus::Connection::session().await,
            Some(address) => {
                zbus::connection::Builder::address(address.as_str())?
                    .build()
                    .await
            }
        }
    }

    /// Run one portal call with the call bound.
    async fn bounded<T, E: Into<ashpd::Error>>(
        what: &'static str,
        call: impl Future<Output = Result<T, E>>,
    ) -> Result<T, ApplyError> {
        match timeout(call, CALL_BOUND).await {
            Some(Ok(value)) => Ok(value),
            Some(Err(error)) => {
                let error: ashpd::Error = error.into();
                Err(ApplyError::Failed(format!("{what}: {error}")))
            }
            None => Err(ApplyError::Failed(format!("{what}: timed out"))),
        }
    }

    /// One session, from creation to its end.
    async fn session(&self) -> End {
        let shared = &self.shared;
        shared.status.set(CaptureStatus::Pending);
        let connection = match self.connect().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(%error, "no session bus for the input capture portal");
                return End::Unavailable;
            }
        };
        if shared.bus_address.is_none() {
            crate::portal::register_on(&connection).await;
        }
        let capture = match InputCapture::with_connection(connection).await {
            Ok(capture) => Arc::new(capture),
            Err(error) => {
                tracing::warn!(%error, "the InputCapture portal is not available");
                return End::Unavailable;
            }
        };
        // Watch the portal's name and signals before creating anything on it.
        let owner = match capture.receive_owner_changed().await {
            Ok(owner) => owner,
            Err(error) => {
                tracing::warn!(%error, "cannot watch the InputCapture portal");
                return End::Unavailable;
            }
        };
        let signals = match capture.receive_all_signals().await {
            Ok(signals) => signals,
            Err(error) => {
                tracing::warn!(%error, "cannot subscribe to InputCapture signals");
                return End::Unavailable;
            }
        };
        let mut owner = pin!(owner);
        let mut signals = pin!(signals);

        let created = {
            let create = self.create_session(&capture);
            let mut create = pin!(create);
            poll_fn(|cx| {
                if self.shared.control.poll(&self.seen, cx.waker()).closing {
                    return Poll::Ready(Err(End::Closing));
                }
                if let Poll::Ready(result) = create.as_mut().poll(cx) {
                    return Poll::Ready(result);
                }
                if owner.as_mut().poll_next(cx).is_ready() {
                    return Poll::Ready(Err(End::PortalLost));
                }
                Poll::Pending
            })
            .await
        };
        let session = match created {
            Ok(session) => session,
            Err(end) => return end,
        };
        let handle = Arc::new(PortalHandle {
            input_capture: Arc::clone(&capture),
            session,
        });
        shared.set_handle(Some(Arc::clone(&handle)));
        let (end, gone) = self.serve(&handle, signals.as_mut(), owner.as_mut()).await;
        self.finish(&handle, gone).await;
        end
    }

    /// Create the session (the consent dialog can take as long as the user does).
    async fn create_session(
        &self,
        capture: &InputCapture,
    ) -> Result<ashpd::desktop::Session<InputCapture>, End> {
        let wanted = Capabilities::Keyboard | Capabilities::Pointer;
        if capture.version() >= 2 {
            let restore = token::read(&self.shared.token_path);
            let session = capture
                .create_session2(CreateSession2Options::default())
                .await
                .map_err(|error| self.failed(&error))?;
            tracing::info!("asking the InputCapture portal to start the session");
            let options = StartOptions::default()
                .set_capabilities(wanted)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(restore);
            let started = match capture.start(&session, None, options).await {
                Ok(request) => request.response(),
                Err(error) => Err(error),
            };
            let granted = started.map_err(|error| self.failed(&error))?;
            if !granted.capabilities().contains(wanted) {
                tracing::warn!("the desktop granted less than keyboard and pointer capture");
                return Err(End::Denied);
            }
            match granted.restore_token() {
                Some(rotated) => {
                    if let Err(error) = token::write(&self.shared.token_path, rotated) {
                        tracing::warn!(
                            %error,
                            "cannot store the input capture restore token; consent will be asked again"
                        );
                    }
                }
                None => tracing::info!("the portal returned no input capture restore token"),
            }
            Ok(session)
        } else {
            tracing::info!(
                "asking the InputCapture portal to create the session (it shows a dialog)"
            );
            let (session, granted) = capture
                .create_session(
                    None,
                    CreateSessionOptions::default().set_capabilities(wanted),
                )
                .await
                .map_err(|error| self.failed(&error))?;
            if !granted.contains(wanted) {
                tracing::warn!("the desktop granted less than keyboard and pointer capture");
                let _ = timeout(session.close(), CLOSE_BOUND).await;
                return Err(End::Denied);
            }
            Ok(session)
        }
    }

    /// Log why creating the session failed and classify it.
    fn failed(&self, error: &ashpd::Error) -> End {
        let end = classify(error);
        match end {
            End::Denied => tracing::info!(%error, "input capture was not allowed"),
            _ => {
                tracing::warn!(%error, "the InputCapture portal is not usable")
            }
        }
        end
    }

    /// Everything after creation: install, then serve until the session ends. Returns how it
    /// ended and whether the portal side is already gone (no `Close` is needed).
    async fn serve<S, O>(
        &self,
        handle: &Arc<PortalHandle>,
        mut signals: Pin<&mut S>,
        mut owner: Pin<&mut O>,
    ) -> (End, bool)
    where
        S: Stream<Item = zbus::Message>,
        O: Stream,
    {
        let shared = &self.shared;
        let closed = match handle.session.receive_closed().await {
            Ok(closed) => closed,
            Err(error) => {
                tracing::warn!(%error, "cannot watch the input capture session");
                return (End::Unavailable, false);
            }
        };
        let mut closed = pin!(closed);
        let mut install = Install::default();
        let mut want_install = true;
        let mut requested = 0u64;
        let mut retries = 0u32;
        let mut retry: Option<Pin<Box<Sleep>>> = None;
        loop {
            shared.panic_point(super::WORKER_THREAD);
            // The work that is due, when nothing is held.
            if want_install && shared.machine_idle() {
                let (generation, wanted) = shared.control.wanted();
                // A set the engine just offered is answered to it; a re-arm is not.
                let fresh = generation != requested;
                match self.install(handle, &mut install, &wanted).await {
                    Ok(()) => {
                        want_install = false;
                        retries = 0;
                        retry = None;
                        requested = generation;
                        shared.control.finish(generation, Ok(()));
                        if install.eis || wanted.is_empty() {
                            shared.status.set(CaptureStatus::Ready);
                        }
                    }
                    Err(Failure::Reset) => return (End::Reset, false),
                    Err(Failure::Fatal) => return (End::Unavailable, false),
                    Err(Failure::Apply(error)) => {
                        requested = generation;
                        shared.control.finish(generation, Err(error.clone()));
                        let rejected = matches!(error, ApplyError::Rejected(_));
                        tracing::warn!(?error, "installing the capture barriers failed");
                        want_install = false;
                        if rejected && fresh {
                            // The engine was told the set is refused and keeps the previous one.
                        } else if retries < INSTALL_RETRIES {
                            // A re-arm whose zones the displays snapshot has not caught up with
                            // yet (a layout change), or a portal call that failed: look again.
                            retries += 1;
                            retry = Some(Box::pin(Sleep::new(INSTALL_RETRY)));
                        } else if !rejected {
                            shared.status.set(CaptureStatus::Unavailable);
                        }
                    }
                }
                continue;
            }

            let event = poll_fn(|cx| {
                let state = shared.control.poll(&self.seen, cx.waker());
                if state.changed || state.closing {
                    return Poll::Ready(Event::Control);
                }
                if closed.as_mut().poll_next(cx).is_ready() {
                    return Poll::Ready(Event::SessionClosed);
                }
                if owner.as_mut().poll_next(cx).is_ready() {
                    return Poll::Ready(Event::OwnerChanged);
                }
                if let Poll::Ready(message) = signals.as_mut().poll_next(cx) {
                    return Poll::Ready(Event::Signal(message));
                }
                if let Some(sleep) = retry.as_mut()
                    && sleep.as_mut().poll(cx).is_ready()
                {
                    return Poll::Ready(Event::Retry);
                }
                Poll::Pending
            })
            .await;

            match event {
                Event::SessionClosed => return (End::Closed, true),
                Event::OwnerChanged => return (End::PortalLost, true),
                Event::Signal(None) => return (End::PortalLost, true),
                Event::Signal(Some(message)) => {
                    if self.on_signal(&message) {
                        want_install = true;
                    }
                }
                Event::Retry => {
                    retry = None;
                    want_install = true;
                }
                Event::Control => {
                    let flags = shared.control.take_flags();
                    if flags.closing {
                        return (End::Closing, false);
                    }
                    if flags.restart {
                        return (End::Restart, false);
                    }
                    if flags.rearm {
                        want_install = true;
                    }
                    if flags.generation != requested {
                        want_install = true;
                        if !shared.machine_idle() {
                            // Installed when the activation is over (A7); the caller is not kept
                            // waiting.
                            shared.control.finish(flags.generation, Ok(()));
                        }
                    }
                }
            }
        }
    }

    /// One signal of the interface. `true`: the barriers must be installed again.
    fn on_signal(&self, message: &zbus::Message) -> bool {
        let header = message.header();
        let Some(member) = header.member() else {
            return false;
        };
        let body = message.body();
        match member.as_str() {
            "Activated" => match body.deserialize::<Activated>() {
                Ok(activated) => {
                    let barrier = match activated.barrier_id() {
                        Some(ActivatedBarrier::Barrier(id)) => Some(id.get()),
                        _ => None,
                    };
                    self.shared.activated(
                        activated.activation_id(),
                        barrier,
                        activated.cursor_position(),
                    );
                    false
                }
                Err(error) => {
                    tracing::warn!(%error, "an unreadable Activated signal; releasing whatever is held");
                    self.shared.release_unknown();
                    false
                }
            },
            "Deactivated" => {
                match body.deserialize::<Deactivated>() {
                    Ok(deactivated) => self.shared.deactivated(deactivated.activation_id()),
                    Err(error) => {
                        tracing::debug!(%error, "ignoring an unreadable Deactivated signal")
                    }
                }
                false
            }
            // The compositor disabled the session (and deactivated any capture before saying so):
            // the barriers are gone from its side.
            "ZonesChanged" | "Disabled" => {
                tracing::info!(
                    signal = member.as_str(),
                    "the capture session was disabled by the compositor"
                );
                self.shared.session_disabled();
                true
            }
            _ => false,
        }
    }

    /// Install the wanted set. Never called while a capture is pending or active.
    async fn install(
        &self,
        handle: &PortalHandle,
        install: &mut Install,
        wanted: &[CapturePortal],
    ) -> Result<(), Failure> {
        let shared = &self.shared;
        let quirks = shared.quirks;
        let capture = &handle.input_capture;
        let session = &handle.session;

        if wanted.is_empty() {
            if install.enabled_once {
                if quirks.close_to_replace {
                    return Err(Failure::Reset);
                }
                // Ignore the answer: a session the compositor already disabled says "not enabled".
                let _ = Self::bounded(
                    "Disable",
                    capture.disable(session, DisableOptions::default()),
                )
                .await;
                shared.set_installed(None, false);
            }
            return Ok(());
        }

        // Barriers change only on a session in its initial state.
        if install.enabled_once && quirks.close_to_replace {
            return Err(Failure::Reset);
        }

        let zones = Self::bounded(
            "GetZones",
            capture.zones(session, GetZonesOptions::default()),
        )
        .await?;
        let zones = zones
            .response()
            .map_err(|error| ApplyError::Failed(format!("GetZones: {error}")))?;
        let list: Vec<Zone> = zones
            .regions()
            .iter()
            .map(|region| Zone {
                x: region.x_offset(),
                y: region.y_offset(),
                width: region.width(),
                height: region.height(),
            })
            .collect();
        let zone_set = zones.zone_set();
        shared.set_zones(list.clone(), zone_set);
        let displays = (shared.displays)();
        let mut plan = barriers::plan(wanted, &displays, &list, zone_set)
            .map_err(|rejection| ApplyError::Rejected(rejection.to_string()))?;

        if install.enabled_once {
            if !quirks.disable_before_barriers {
                let _ = Self::bounded(
                    "Disable",
                    capture.disable(session, DisableOptions::default()),
                )
                .await;
            }
            shared.set_installed(None, false);
        }

        let lines: Vec<Barrier> = plan
            .barriers
            .iter()
            .filter_map(|barrier| {
                std::num::NonZeroU32::new(barrier.id).map(|id| {
                    Barrier::new(
                        id,
                        BarrierPosition::new(
                            barrier.line.x1,
                            barrier.line.y1,
                            barrier.line.x2,
                            barrier.line.y2,
                        ),
                    )
                })
            })
            .collect();
        let request = Self::bounded(
            "SetPointerBarriers",
            capture.set_pointer_barriers(
                session,
                &lines,
                zone_set,
                SetPointerBarriersOptions::default(),
            ),
        )
        .await?;
        let answer = request
            .response()
            .map_err(|error| ApplyError::Failed(format!("SetPointerBarriers: {error}")))?;
        let failed: Vec<u32> = answer.failed_barriers().iter().map(|id| id.get()).collect();
        if !failed.is_empty() {
            tracing::warn!(
                refused = failed.len(),
                of = plan.barriers.len(),
                "the desktop refused some capture barriers"
            );
            plan.barriers
                .retain(|barrier| !failed.contains(&barrier.id));
            if plan.barriers.is_empty() {
                // The previous barriers were removed above, so this is no clean refusal.
                return Err(ApplyError::Failed("the desktop refused every barrier".into()).into());
            }
        }

        // `Enable` needs the EIS connection (mutter: "Not connected to EIS"), made once.
        if !install.eis {
            let fd = Self::bounded(
                "ConnectToEIS",
                capture.connect_to_eis(session, ConnectToEISOptions::default()),
            )
            .await?;
            let (reply, answer) = std::sync::mpsc::sync_channel(1);
            shared.eis.push(EisCmd::Attach { fd, reply });
            let deadline = Instant::now() + ATTACH_BOUND;
            let attached = loop {
                match answer.recv_timeout(Duration::from_millis(20)) {
                    Ok(result) => break result,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if Instant::now() >= deadline || shared.control.is_closing() {
                            break Err(PlatformError::Timeout);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        break Err(PlatformError::Backend("the EIS thread stopped".into()));
                    }
                }
            };
            if let Err(error) = attached {
                // Without the receiver nothing can be captured: a new session would fail the same
                // way (and ask again), so this ends the backend's use.
                tracing::warn!(%error, "the capture EIS connection failed");
                return Err(Failure::Fatal);
            }
            install.eis = true;
        }

        Self::bounded("Enable", capture.enable(session, EnableOptions::default())).await?;
        install.enabled_once = true;
        shared.set_installed(Some(plan), true);
        Ok(())
    }

    /// The session is over: nobody can release or receive through it any more.
    async fn finish(&self, handle: &Arc<PortalHandle>, gone: bool) {
        let shared = &self.shared;
        shared.set_handle(None);
        shared.eis.push(EisCmd::Detach);
        shared.session_ended();
        if !gone {
            // Closing also dismisses a consent dialog that is still up. A session the portal
            // already closed, or a portal that is gone, needs no call.
            match timeout(handle.session.close(), CLOSE_BOUND).await {
                Some(Ok(())) => {}
                Some(Err(error)) => tracing::debug!(%error, "closing the capture session failed"),
                None => tracing::warn!("closing the capture session timed out"),
            }
        }
    }
}

/// Why an install did not finish.
enum Failure {
    /// The portal needs the session closed and created again (the `close_to_replace` quirk).
    Reset,
    /// The session cannot be used and another one would fail the same way (and ask again).
    Fatal,
    Apply(ApplyError),
}

impl From<ApplyError> for Failure {
    fn from(error: ApplyError) -> Failure {
        Failure::Apply(error)
    }
}

/// A user's "no" is `Denied`; every other way of not getting a session is `Unavailable`.
fn classify(error: &ashpd::Error) -> End {
    match error {
        ashpd::Error::Response(_)
        | ashpd::Error::Portal(PortalError::Cancelled(_) | PortalError::NotAllowed(_)) => {
            End::Denied
        }
        _ => End::Unavailable,
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
            assert_eq!(classify(&error), End::Denied, "{error}");
        }
    }

    #[test]
    fn missing_or_failing_portals_are_unavailable() {
        for error in [
            ashpd::Error::RequiresVersion(2, 1),
            ashpd::Error::NoResponse,
            ashpd::Error::Portal(PortalError::Failed(String::new())),
            ashpd::Error::Zbus(zbus::Error::Failure(String::new())),
        ] {
            assert_eq!(classify(&error), End::Unavailable, "{error}");
        }
    }
}
