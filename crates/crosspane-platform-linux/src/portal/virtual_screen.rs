//! A Mutter virtual monitor through the ScreenCast portal (WP-G2.4, task A): the "twin" a
//! projected window is parked on.
//!
//! **What it is.** A ScreenCast session whose source is `SourceType::Virtual` (`types = 4`), plus a
//! PipeWire consumer that offers **exactly one size**. Mutter creates the virtual monitor when the
//! consumer negotiates a format and gives it the negotiated size, so the caller decides the
//! monitor's size ([`VirtualScreen::open`], [`VirtualScreen::resize`]) and the monitor lives as long
//! as the value does (dropping it closes the session). The portal stream has no position or size:
//! where the monitor sits in the compositor's layout is the caller's business (and not this
//! module's: it makes no `org.gnome.Mutter.*` call). Only the ScreenCast portal is used.
//!
//! **Consent** is caller-prepared. The first session shows the desktop's "share" dialog and returns
//! a restore token (`PersistMode::ExplicitlyRevoked`); a later session started with that token
//! (rotated on every start, stored 0600 at [`VirtualScreenConfig::token_path`]) is silent.
//! [`prepare_consent`] asks once, at agent start, off the input path, and closes the session
//! without ever connecting PipeWire, so it creates no monitor. [`VirtualScreen::open`] never
//! shows or waits for a dialog: with no token it is `Unsupported` without a single portal call,
//! and a `Start` that has not answered in 5 s (the token was revoked and a dialog is up) is
//! abandoned: the session is closed, the token file removed, and `Unsupported` returned, so the
//! next agent start asks again.
//!
//! **Capture.** [`VirtualScreen`] is a [`FrameCapture`] with the monitor capture's frame semantics
//! (`PortalScreenCast`): CPU frames in BGRA, damage, crop in device pixels, `max_fps`, the gate
//! (nothing is read while it is closed; streams end `Blocked`; `start` is `Locked`). It serves
//! `CaptureTarget::Display(_)` for any display id (the caller routes: this screen has no display
//! id of its own) and `CaptureTarget::Window(_)` is `NotFound`. One PipeWire stream feeds every
//! capture; a capture started later still gets the newest image at once. The stream offers 60
//! frames per second as its maximum (a Mutter virtual monitor runs at 60 Hz), so a capture's
//! `max_fps` above 60 is no faster than 60. After the gate has been closed the stream's newest
//! image is gone and a new capture waits for the next repaint of a static screen. See `stream`
//! for the PipeWire thread.
//!
//! **Resizing.** `resize` offers the new fixed size on the running stream (`update_params`) and
//! returns when the stream has renegotiated to it, at most 3 s (`Timeout`). Running captures keep
//! going and get whole frames at the new size; their crops are the caller's to move
//! ([`FrameCapture::set_crop`]). After a `Timeout` the stream may still reach the new size later
//! ([`VirtualScreen::size`] tells); a caller that cannot live with that drops the screen.
//!
//! **Loss.** The user stopping the share from the top bar (the portal's `Closed` signal), the
//! portal losing its bus name, or PipeWire failing ends the screen: every capture ends
//! `TargetGone`, [`VirtualScreen::is_live`] turns false and [`VirtualScreen::on_lost`]'s callback
//! runs once, on the backend thread. Dropping the screen (`Requested`, no callback) closes the
//! session; Mutter then removes the monitor and restores its stored layout by itself.
//!
//! # Implementation
//!
//! Three threads, like the monitor capture: the caller's, the session worker (`worker`, thread
//! `crosspane-portal-virtual`, `zbus::block_on`, no tokio) and the PipeWire thread (`stream`,
//! thread `crosspane-pipewire-virtual`). The handle talks to the PipeWire thread with commands that
//! each wait at most 1.8 s, and to the worker through a shared state with a condition variable
//! (`worker::Shared`). The capture state, the format pods, the frame bookkeeping and the token file
//! are the monitor capture's own (`portal::screencast`), shared by visibility only.
//!
//! **Tests.** `format` has pure tests of the fixed-size pod; `tests` runs the whole handle against
//! the fake ScreenCast portal of `screencast::fake_portal` on a private `dbus-daemon` (the options
//! sent, the token, the consent run, a start that never answers, `Closed`) and, together with a
//! private `pipewire` daemon and a synthetic range-size producer (`screencast::private_server`'s
//! server), the whole data path including `resize`. Neither is Mutter: the real negotiation, the
//! monitor appearing and its size, and the renegotiation on resize remain live checks.

mod format;
// Compiled again on purpose (see `portal::screencast`): the timers of the worker's single-threaded
// executor.
#[allow(clippy::duplicate_mod)]
#[path = "session/sleep.rs"]
mod sleep;
mod stream;
#[cfg(test)]
mod tests;
mod worker;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crosspane_platform::{
    CaptureTarget, EventSink, FrameCapture, FrameEvent, IoGate, PlatformError, StreamId,
};
use crosspane_types::geom::{PixelRect, PixelSize};

use self::stream::{Command, StartRequest, StreamThread};
use self::worker::{Job, Shared, Timeouts};
use super::screencast::frames::validate_crop;
use super::screencast::token;

/// How long a command to the PipeWire thread may take, inside the frozen two-second bound.
const CALL_TIMEOUT: Duration = Duration::from_millis(1800);
/// How long a resize may take in all, from the command to the renegotiated format.
const RESIZE_TIMEOUT: Duration = Duration::from_secs(3);

/// What the virtual screen is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualScreenConfig {
    /// `<state_dir>/portal-virtual.token` (0600, rotated on every start;
    /// `portal/screencast/token.rs`).
    pub token_path: PathBuf,
}

/// Consent-only run for agent start: when no token file exists, start a VIRTUAL session (the
/// desktop shows its dialog), store the restore token, close the session without connecting
/// PipeWire. With a token present it does nothing (no portal call at all). Blocking; bounded by
/// `wait` (the dialog is the user's): when `wait` runs out the dialog is dismissed and the answer
/// is `Ok(false)`.
///
/// `Ok(true)` token stored or already present, `Ok(false)` denied, cancelled, not answered in
/// time, or granted without a restore token, `Err` no portal or failure.
pub fn prepare_consent(
    config: &VirtualScreenConfig,
    wait: Duration,
) -> Result<bool, PlatformError> {
    worker::consent(config.token_path.clone(), wait, None)
}

/// One Mutter virtual monitor, alive while this value lives (Drop closes the session; the monitor
/// disappears). Frames only while `gate` is open; streams end `Blocked` when it closes.
pub struct VirtualScreen {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    worker: Option<JoinHandle<()>>,
    // Dropped after the worker has been joined: it ends the captures still running.
    pipewire: StreamThread,
    gate: Arc<IoGate>,
    next_id: u64,
}

impl std::fmt::Debug for VirtualScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualScreen")
            .field("size", &self.size())
            .field("live", &self.is_live())
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl VirtualScreen {
    /// Start a VIRTUAL session from the stored token (never without one: `Unsupported`), connect a
    /// PipeWire consumer whose EnumFormat offers exactly `size` (device px), and return once the
    /// stream is negotiated at that size (the monitor exists). Start must answer within 5 s or the
    /// session is closed, the token file removed, and `Unsupported` returned (a dialog would be
    /// up). Negotiation bounded at 3 s (`Timeout`). `Locked` while the gate is closed.
    pub fn open(
        gate: Arc<IoGate>,
        config: VirtualScreenConfig,
        size: PixelSize,
    ) -> Result<VirtualScreen, PlatformError> {
        Self::open_on(gate, config, size, None, Timeouts::default())
    }

    /// [`open`](Self::open) on a given bus address instead of the session bus, with the bounds
    /// chosen.
    fn open_on(
        gate: Arc<IoGate>,
        config: VirtualScreenConfig,
        size: PixelSize,
        bus_address: Option<String>,
        timeouts: Timeouts,
    ) -> Result<VirtualScreen, PlatformError> {
        if !gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if !format::size_supported(size) {
            return Err(PlatformError::Backend(
                "virtual screen: size out of range".into(),
            ));
        }
        // No token, no consent: say so before any thread or portal call.
        if token::read(&config.token_path).is_none() {
            return Err(PlatformError::Unsupported(
                "no stored consent for a virtual screen",
            ));
        }
        let shared = Shared::new(size);
        let (commands, receiver) = mpsc::channel();
        let pipewire = StreamThread::spawn(receiver, Arc::clone(&gate), Arc::clone(&shared))?;
        let worker = worker::spawn(Job {
            shared: Arc::clone(&shared),
            commands: commands.clone(),
            token_path: config.token_path,
            bus_address,
            size,
            timeouts,
        })?;
        // From here on a failure drops the screen, which closes the session and stops the threads.
        let screen = VirtualScreen {
            shared,
            commands,
            worker: Some(worker),
            pipewire,
            gate,
            next_id: 1,
        };
        screen.shared.await_ready(size, &timeouts)?;
        Ok(screen)
    }

    /// Negotiated size (device px): the last size the stream settled on.
    pub fn size(&self) -> PixelSize {
        self.shared.size()
    }

    /// Renegotiate to `size` (update_params with a single fixed size); returns once the stream runs
    /// at it (≤ 3 s, `Timeout`). Running streams keep going; their crop is the caller's.
    pub fn resize(&mut self, size: PixelSize) -> Result<(), PlatformError> {
        if !format::size_supported(size) {
            return Err(PlatformError::Backend(
                "virtual screen: size out of range".into(),
            ));
        }
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if !self.is_live() {
            return Err(PlatformError::NotFound);
        }
        if self.size() == size {
            return Ok(());
        }
        let deadline = Instant::now() + RESIZE_TIMEOUT;
        self.call(deadline.min(Instant::now() + CALL_TIMEOUT), |reply| {
            Command::Resize { size, reply }
        })?;
        self.shared
            .await_size(size, deadline.saturating_duration_since(Instant::now()))
    }

    /// False once the session closed (user stopped it, portal gone) or PipeWire failed.
    pub fn is_live(&self) -> bool {
        self.shared.is_live()
    }

    /// Called once, on the backend thread, when the screen is lost (not on Drop). Must not block.
    /// A screen that is already lost calls it at once, on the caller's thread.
    pub fn on_lost(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        self.shared.set_on_lost(callback);
    }

    /// Send `command` (built around a fresh reply channel) and wait for the answer until
    /// `deadline`.
    fn call(
        &self,
        deadline: Instant,
        build: impl FnOnce(SyncSender<Result<(), PlatformError>>) -> Command,
    ) -> Result<(), PlatformError> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.commands
            .send(build(reply))
            .map_err(|_| unavailable())?;
        match answer.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(PlatformError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(unavailable()),
        }
    }
}

fn unavailable() -> PlatformError {
    PlatformError::Backend("virtual screen: the PipeWire thread is gone".into())
}

impl Drop for VirtualScreen {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            // The worker's last acts are to tell the PipeWire thread to end the captures and to
            // close the portal session (which removes the monitor).
            self.shared.close_and_join(worker);
        }
        self.pipewire.stop();
    }
}

/// Serves `CaptureTarget::Display(_)` (whatever id: the caller routes); `Window` is `NotFound`.
/// Same frame semantics as `PortalScreenCast` (CPU frames, damage, crop, max_fps, gate).
impl FrameCapture for VirtualScreen {
    fn start(
        &mut self,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError> {
        let deadline = Instant::now() + CALL_TIMEOUT;
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if max_fps == 0 {
            return Err(PlatformError::Backend(
                "virtual screen: max_fps must be positive".into(),
            ));
        }
        match target {
            CaptureTarget::Display(_) => {}
            // A virtual screen has no windows of its own.
            _ => return Err(PlatformError::NotFound),
        }
        validate_crop(crop)?;
        if !self.is_live() {
            return Err(PlatformError::NotFound);
        }
        let id = StreamId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| PlatformError::Backend("virtual screen: stream ids exhausted".into()))?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let result = self.call(deadline, |reply| {
            Command::Start(Box::new(StartRequest {
                id,
                crop,
                max_fps,
                sink,
                cancelled: Arc::clone(&cancelled),
                reply,
            }))
        });
        if result.is_err() {
            // A request that is still queued must not make a stream nobody knows about.
            cancelled.store(true, std::sync::atomic::Ordering::Release);
        }
        result.map(|()| id)
    }

    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        validate_crop(crop)?;
        self.call(Instant::now() + CALL_TIMEOUT, |reply| Command::SetCrop {
            stream,
            crop,
            reply,
        })
    }

    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
        match self.call(Instant::now() + CALL_TIMEOUT, |reply| Command::Stop {
            stream,
            reply,
        }) {
            // Without the PipeWire thread there is no stream left to stop.
            Err(PlatformError::Backend(_)) | Ok(()) => Ok(()),
            Err(error) => Err(error),
        }
    }
}
