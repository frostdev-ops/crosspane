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
//! DMA-BUF in this package), and delivers each new buffer as a CPU [`Frame`] with the crop
//! applied (`set_crop`, device pixels, clamped). The queue toward the sink is newest-wins, never
//! unbounded. `CaptureTarget::Window` is `Unsupported` (window identity comes from the Shell
//! bridge, never from portal metadata). `stop` disconnects the stream; it is idempotent.
//!
//! **Gate** (frozen `FrameCapture` contract): no frames while the [`IoGate`] is closed, every
//! stream ends with `Blocked` when it closes, and `start` refuses with `Locked` while closed.
//! Frames are delivered at most `max_fps` per second. A portal `Closed` signal (the user pressed Stop) ends every stream with the frozen
//! "ended" reason and the session stays closed until [`PortalScreenCast::restart`]; only a
//! portal bus-name owner change gets one silent retry with the stored token.

use std::path::PathBuf;
use std::sync::Arc;

use crosspane_platform::{
    CaptureTarget, EventSink, FrameCapture, FrameEvent, IoGate, PlatformError, StreamId,
};
use crosspane_types::geom::PixelRect;

use super::eis::DisplaysFn;

/// What the ScreenCast session is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenCastConfig {
    /// Where the restore token lives, e.g. `<state_dir>/portal-screencast.token`.
    pub token_path: PathBuf,
}

/// Monitor capture through the ScreenCast portal.
#[derive(Debug)]
pub struct PortalScreenCast {}

impl PortalScreenCast {
    /// Start the session worker (which may show the consent dialog once) and the PipeWire
    /// thread; returns within 2 s without waiting for consent.
    pub fn new(
        gate: Arc<IoGate>,
        config: ScreenCastConfig,
        displays: DisplaysFn,
    ) -> Result<PortalScreenCast, PlatformError> {
        let _ = (gate, config, displays);
        Err(PlatformError::Unsupported(
            "ScreenCast capture not implemented yet",
        ))
    }

    /// Whether a started session with at least one mapped stream exists now.
    pub fn is_live(&self) -> bool {
        false
    }

    /// Close the session and ask again (may show the dialog).
    pub fn restart(&self) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(
            "ScreenCast capture not implemented yet",
        ))
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
        let _ = (target, crop, max_fps, sink);
        Err(PlatformError::Unsupported(
            "ScreenCast capture not implemented yet",
        ))
    }

    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        let _ = (stream, crop);
        Err(PlatformError::NotFound)
    }

    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
        let _ = stream;
        Ok(())
    }
}
