//! `FrameCapture` through the ScreenCast portal and PipeWire (WP-G2.2), compositor-neutral.
//!
//! **Session.** One worker owns one ScreenCast session (`ashpd`, `zbus::block_on`) that selects
//! every monitor (`SourceType::Monitor`, `multiple = true`, cursor mode `Hidden`),
//! `PersistMode::ExplicitlyRevoked`, with the restore token at `token_path` (read at each start,
//! rotated token written 0600 via temp file + rename, never logged; the same rules as
//! `portal::session`). Consent is caller-prepared: the session starts at construction, so the
//! desktop's "share screen" dialog appears at agent startup at most once; trait calls never wait
//! for it. It then calls `OpenPipeWireRemote` and keeps that fd for its PipeWire core.
//!
//! **Streams to displays.** Each portal stream reports `position` and `size` in logical
//! coordinates. Stream *k* belongs to the display whose `logical_origin` equals the position and
//! whose logical size (`pixel_size / scale`, rounded) equals the size, from the displays snapshot.
//! No match, or two matches, leaves that stream unmapped (logged); never guessed.
//!
//! **Capture.** `start(CaptureTarget::Display(id), crop, max_fps, sink)` connects a PipeWire video stream to
//! that display's node on a PipeWire thread (one main loop for all streams), negotiating
//! BGRx/BGRA (also accept RGBx/RGBA/xRGB and convert to BGRA), SHM/MemFd buffers only (no
//! DMA-BUF in this package), and delivers each new buffer as a CPU [`Frame`](crosspane_platform::Frame) with the crop
//! applied (`set_crop`, device pixels, clamped). The queue toward the sink is newest-wins, never
//! unbounded. `CaptureTarget::Window` is `Unsupported` (window identity comes from the Shell
//! bridge, never from portal metadata). `stop` disconnects the stream; it is idempotent.
//!
//! **Gate** (frozen `FrameCapture` contract): no frames while the [`IoGate`] is closed, every
//! stream ends with `Blocked` when it closes, and `start` refuses with `Locked` while closed.
//! Frames are delivered at most `max_fps` per second. A portal `Closed` signal (the user pressed Stop) ends every stream with the frozen
//! "ended" reason and the session stays closed until [`PortalScreenCast::restart`]; only a
//! portal bus-name owner change gets one silent retry with the stored token.
//!
//! # Implementation
//!
//! **Threads.** The session worker (`worker`, thread `crosspane-portal-screencast`) and the
//! PipeWire thread (`capture`, thread `crosspane-pipewire-capture`) are the only threads. The
//! worker obtains the portal's PipeWire fd and hands it to the PipeWire thread, which owns the
//! main loop, the core and every stream; the handle talks to it with commands that each wait at
//! most 1.8 s. `start` answers once the PipeWire stream is connected; formats and frames follow
//! asynchronously. Dropping the handle closes the session (the streams end `Requested`), joins the
//! worker (bounded), then stops the PipeWire thread (bounded).
//!
//! **Why `Closed` is `Blocked`.** The frozen `StreamEndReason::Blocked` covers "permission was
//! revoked", which is what the user's Stop is, and the engine then ends the projection as locked
//! and never re-arms it. Losing the portal or the PipeWire connection, `restart`, and a stream
//! error end streams with `Failed`; a node that disappears with its display is `TargetGone`; a
//! dropped handle is `Requested`.
//!
//! **What `start` answers while there is no session.** `InteractionRequired` while the dialog may
//! be up or after the session closed (the caller decides whether to ask again with `restart`),
//! `PermissionDenied(ScreenRecording)` after a "no", `Unsupported` without a portal, `Locked` with
//! the gate closed, `NotFound` for a display with no (or an ambiguous) stream.
//!
//! **Sinks.** Events reach the sink on the PipeWire thread. A sink must not block and must not
//! call back into the capture handle.
//!
//! **Pure parts** are in `streams` (display matching), `format` (the pods asked for and read),
//! `pixels` (conversion to BGRA), `frames` (crop, damage, pacing, timestamps) and `token`.
//! `lifecycle` (the epoch state machine) and `sleep` (timers) are the RemoteDesktop session's own
//! files, compiled here by path so both sessions follow the same rules.
//!
//! **Tests.** Beside the unit tests of those parts, `fake_portal` runs the whole handle against a
//! fake ScreenCast portal on a private `dbus-daemon` (the options sent, the streams parsed, the
//! token, `Closed`, restarts), and `private_server` runs the PipeWire data path against a private
//! `pipewire` daemon with a synthetic producer (negotiation, padded rows, memfd and plain memory,
//! crops, pacing, the gate, a vanishing node). Both skip when their binary is missing and never
//! touch the desktop's portal or PipeWire server; neither is a compositor, so mutter's and KWin's
//! own buffers and timing remain a live check.

// The internals `portal::virtual_screen` shares (the capture state, the format pods, frame
// bookkeeping, the token file) are visible to `portal`; they keep their behaviour.
pub(super) mod capture;
#[cfg(test)]
pub(super) mod fake_portal;
pub(super) mod format;
pub(super) mod frames;
// Compiled a second time on purpose (see the module docs): `lifecycle` names this module's own
// `SessionStatus` through `super`, so it cannot be shared as one module with `portal::session`.
#[allow(clippy::duplicate_mod)]
#[path = "session/lifecycle.rs"]
mod lifecycle;
mod pixels;
#[cfg(test)]
pub(super) mod private_server;
#[allow(clippy::duplicate_mod)]
#[path = "session/sleep.rs"]
mod sleep;
mod streams;
pub(super) mod token;
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
use crosspane_types::geom::PixelRect;

use self::capture::{Command, PipeWireThread, StartRequest};
use self::streams::Unmapped;
use self::worker::Shared;
use super::eis::DisplaysFn;

/// How long a command to the PipeWire thread may take, inside the frozen two-second bound.
const CALL_TIMEOUT: Duration = Duration::from_millis(1800);

/// The session's state (the vocabulary of the shared `lifecycle` state machine).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionStatus {
    /// Starting: creating the session or waiting for the user's consent.
    Pending,
    /// Started with at least one monitor stream; its PipeWire fd is (or was) attached.
    Active { epoch: u64 },
    /// The user denied or cancelled the dialog, or shared no monitor. Not retried until `restart`.
    Denied,
    /// The epoch's session ended (revoked, portal gone, `close`).
    Closed { epoch: u64 },
    /// No ScreenCast portal on this desktop, or it fails.
    Unavailable,
}

/// What the ScreenCast session is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenCastConfig {
    /// Where the restore token lives, e.g. `<state_dir>/portal-screencast.token`.
    pub token_path: PathBuf,
}

/// Monitor capture through the ScreenCast portal.
pub struct PortalScreenCast {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    worker: Option<JoinHandle<()>>,
    // Dropped after the worker has been joined: it ends the streams still running.
    pipewire: PipeWireThread,
    gate: Arc<IoGate>,
    displays: DisplaysFn,
    next_id: u64,
}

impl std::fmt::Debug for PortalScreenCast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalScreenCast")
            .field("shared", &self.shared)
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl PortalScreenCast {
    /// Start the session worker (which may show the consent dialog once) and the PipeWire
    /// thread; returns within 2 s without waiting for consent.
    pub fn new(
        gate: Arc<IoGate>,
        config: ScreenCastConfig,
        displays: DisplaysFn,
    ) -> Result<PortalScreenCast, PlatformError> {
        Self::new_on(gate, config, displays, None)
    }

    /// [`new`](Self::new) on a given bus address instead of the session bus.
    fn new_on(
        gate: Arc<IoGate>,
        config: ScreenCastConfig,
        displays: DisplaysFn,
        bus_address: Option<String>,
    ) -> Result<PortalScreenCast, PlatformError> {
        let shared = Shared::new();
        let (commands, receiver) = mpsc::channel();
        let pipewire = PipeWireThread::spawn(
            receiver,
            Arc::clone(&gate),
            Arc::clone(&displays),
            Arc::clone(&shared),
        )?;
        let worker = worker::spawn(
            Arc::clone(&shared),
            config.token_path,
            commands.clone(),
            bus_address,
        )?;
        Ok(PortalScreenCast {
            shared,
            commands,
            worker: Some(worker),
            pipewire,
            gate,
            displays,
            next_id: 1,
        })
    }

    /// Whether a started session with at least one mapped stream exists now.
    pub fn is_live(&self) -> bool {
        let Ok(active) = self.shared.active() else {
            return false;
        };
        streams::mapped_count(&active.streams, &(self.displays)()) > 0
    }

    /// Close the session and ask again (may show the dialog).
    pub fn restart(&self) -> Result<(), PlatformError> {
        self.shared.restart()
    }

    /// Send `command` (built around a fresh reply channel) and wait for the answer, bounded.
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
    PlatformError::Backend("screencast: the PipeWire thread is gone".into())
}

impl Drop for PortalScreenCast {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            // The worker's last act is to tell the PipeWire thread to end the streams.
            self.shared.close_and_join(worker);
        }
        self.pipewire.stop();
    }
}

impl FrameCapture for PortalScreenCast {
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
                "screencast: max_fps must be positive".into(),
            ));
        }
        let CaptureTarget::Display(wanted) = target else {
            return Err(PlatformError::Unsupported(
                "ScreenCast captures displays; windows come from the Shell bridge",
            ));
        };
        frames::validate_crop(crop)?;
        let active = self.shared.active()?;
        let snapshot = (self.displays)();
        let stream =
            streams::stream_for(&active.streams, &snapshot, wanted).map_err(|unmapped| {
                tracing::warn!(
                    display = wanted.0,
                    ?unmapped,
                    streams = active.streams.len(),
                    "no ScreenCast stream for this display"
                );
                match unmapped {
                    Unmapped::NoDisplay | Unmapped::NoStream => PlatformError::NotFound,
                    Unmapped::Ambiguous => PlatformError::Backend(
                        "screencast: more than one stream or display has this geometry".into(),
                    ),
                }
            })?;
        let device_size = snapshot
            .iter()
            .find(|display| display.id == wanted)
            .map(|display| display.geometry.pixel_size)
            .ok_or(PlatformError::NotFound)?;
        let id = StreamId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| PlatformError::Backend("screencast: stream ids exhausted".into()))?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let result = self.call(deadline, |reply| {
            Command::Start(Box::new(StartRequest {
                id,
                epoch: active.epoch,
                node_id: stream.node_id,
                display: wanted,
                device_size,
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
        frames::validate_crop(crop)?;
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
