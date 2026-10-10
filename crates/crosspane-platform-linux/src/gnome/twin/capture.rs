//! The capture router of the twin (WP-G2.4 B2): a [`FrameCapture`] that serves
//! `CaptureTarget::Display(twin id)` from the twin's PipeWire stream and sends everything else to
//! the capture it wraps (the monitor and window capture, unchanged).
//!
//! - **Stream ids.** A virtual screen numbers its streams from 1, like the monitor capture, and
//!   the window capture uses ids from 2^62. Twin streams are renumbered from [`TWIN_STREAM_BASE`]
//!   (2^61), so the three ranges never meet and `set_crop` and `stop` find the owner from the id
//!   alone. Every event of a twin stream is re-addressed with the outer id by [`Retag`], which is
//!   given to the screen before the stream starts, so no event can arrive with the wrong id.
//! - **Endings.** A stream that ends `Requested` without anyone having stopped it (the twin was
//!   dropped under it: the linger ran out, or a failed growth tore it down) ends `TargetGone`
//!   instead, the reason for a display that went away. A twin that is lost ends its streams
//!   `TargetGone` itself.
//! - **Gaps.** A stream held by this router does not keep a dropped twin alive: it knows its
//!   screen weakly, `stop` on a stream whose screen is gone is `Ok`, and `set_crop` is `NotFound`.
//! - **First frame.** A stream that has had no frame a moment after it started is reported to the
//!   twin, which nudges the screen (see `GnomeTwin::watch_first_frame`).

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use crosspane_platform::{
    CaptureTarget, EventSink, FrameCapture, FrameEvent, PlatformError, StreamEndReason, StreamId,
};
use crosspane_types::geom::PixelRect;

use super::{GnomeTwin, ScreenHandle, lock};

/// The first id of a twin stream; window streams start at 2^62, monitor streams below this.
pub const TWIN_STREAM_BASE: u64 = 1 << 61;
/// One past the last twin stream id.
const TWIN_STREAM_END: u64 = 1 << 62;

fn is_twin_stream(stream: StreamId) -> bool {
    (TWIN_STREAM_BASE..TWIN_STREAM_END).contains(&stream.0)
}

/// A twin stream as the router knows it.
struct Routed {
    screen: Weak<std::sync::Mutex<Box<dyn super::Screen>>>,
    /// The id the screen gave the stream.
    inner: StreamId,
    /// Set before a stop is asked for, so the `Requested` ending that follows is passed on as it is.
    stopping: Arc<AtomicBool>,
    /// Set when the stream has had its first frame, or is over: nothing is left to nudge for.
    settled: Arc<AtomicBool>,
}

/// The router: see the module documentation.
pub struct TwinCapture {
    twin: GnomeTwin,
    inner: Box<dyn FrameCapture>,
    streams: HashMap<StreamId, Routed>,
    next: u64,
}

impl fmt::Debug for TwinCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwinCapture")
            .field("twin_streams", &self.streams.len())
            .finish_non_exhaustive()
    }
}

impl TwinCapture {
    pub(super) fn new(twin: GnomeTwin, inner: Box<dyn FrameCapture>) -> TwinCapture {
        TwinCapture {
            twin,
            inner,
            streams: HashMap::new(),
            next: 0,
        }
    }

    fn routed(&self, stream: StreamId) -> Result<(ScreenHandle, StreamId), PlatformError> {
        let routed = self.streams.get(&stream).ok_or(PlatformError::NotFound)?;
        let screen = routed.screen.upgrade().ok_or(PlatformError::NotFound)?;
        Ok((screen, routed.inner))
    }
}

impl FrameCapture for TwinCapture {
    fn start(
        &mut self,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError> {
        let screen = match target {
            CaptureTarget::Display(display) => self.twin.screen_for(display),
            _ => None,
        };
        let Some(screen) = screen else {
            return self.inner.start(target, crop, max_fps, sink);
        };
        // Streams whose twin is gone are over; forget them.
        self.streams
            .retain(|_, routed| routed.screen.strong_count() > 0);
        let outer = StreamId(
            TWIN_STREAM_BASE
                .checked_add(self.next)
                .filter(|id| *id < TWIN_STREAM_END)
                .ok_or_else(|| PlatformError::Backend("twin stream ids exhausted".into()))?,
        );
        self.next += 1;
        let stopping = Arc::new(AtomicBool::new(false));
        let settled = Arc::new(AtomicBool::new(false));
        let retag = Arc::new(Retag {
            outer,
            sink,
            stopping: Arc::clone(&stopping),
            settled: Arc::clone(&settled),
        });
        let inner = lock(&screen).start(target, crop, max_fps, retag)?;
        self.streams.insert(
            outer,
            Routed {
                screen: Arc::downgrade(&screen),
                inner,
                stopping,
                settled: Arc::clone(&settled),
            },
        );
        self.twin.watch_first_frame(settled);
        Ok(outer)
    }

    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        if !is_twin_stream(stream) {
            return self.inner.set_crop(stream, crop);
        }
        let (screen, inner) = self.routed(stream)?;
        lock(&screen).set_crop(inner, crop)
    }

    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
        if !is_twin_stream(stream) {
            return self.inner.stop(stream);
        }
        let routed = self
            .streams
            .remove(&stream)
            .ok_or(PlatformError::NotFound)?;
        routed.stopping.store(true, Ordering::SeqCst);
        routed.settled.store(true, Ordering::SeqCst);
        match routed.screen.upgrade() {
            Some(screen) => lock(&screen).stop(routed.inner),
            // The twin is gone and the stream ended with it.
            None => Ok(()),
        }
    }
}

/// Re-addresses the events of one twin stream with its outer id.
struct Retag {
    outer: StreamId,
    sink: Arc<dyn EventSink<FrameEvent>>,
    stopping: Arc<AtomicBool>,
    /// See [`Routed::settled`].
    settled: Arc<AtomicBool>,
}

impl EventSink<FrameEvent> for Retag {
    fn send(&self, event: FrameEvent) {
        let stream = self.outer;
        let event = match event {
            FrameEvent::Frame { frame, .. } => {
                self.settled.store(true, Ordering::SeqCst);
                FrameEvent::Frame { stream, frame }
            }
            FrameEvent::Ended { reason, .. } => {
                self.settled.store(true, Ordering::SeqCst);
                let unasked =
                    reason == StreamEndReason::Requested && !self.stopping.load(Ordering::SeqCst);
                FrameEvent::Ended {
                    stream,
                    reason: if unasked {
                        StreamEndReason::TargetGone
                    } else {
                        reason
                    },
                }
            }
            FrameEvent::Cursor { cursor, .. } => FrameEvent::Cursor { stream, cursor },
            FrameEvent::CursorDefault { .. } => FrameEvent::CursorDefault { stream },
            // An event this router does not know cannot be re-addressed.
            _ => return,
        };
        self.sink.send(event);
    }
}
