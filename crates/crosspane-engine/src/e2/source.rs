//! Lifecycle, focus guard and content-to-display input mapping on the window owner.

use std::collections::BTreeMap;
use std::time::Duration;

use crosspane_input::Held;
use crosspane_platform::{
    CaptureTarget, Parked, ParkingKind as PlatformParking, StreamEndReason, StreamId, WindowInfo,
    WindowRole, WindowState,
};
use crosspane_protocol::msg::{Capability, Refusal};
use crosspane_protocol::projection::{
    BrowsableWindow, MAX_BROWSE_WINDOWS, ParkingKind, ProjInput, ProjectionEndReason as Reason,
    ProjectionMessage as Message, ProxyPlacement, WindowSummary,
};
use crosspane_types::geom::{PixelSize, PointDevice, RectLogical};
use crosspane_types::id::{DisplayId, NodeId, ProjectionId, WindowId};
use crosspane_types::time::MonoTime;

use super::{E2, GRACE, Placement, TwinHome, send};
use crate::io::{Failure, InjectCmd, InjectId, Notice, Output, ProjectionKey};

const START_TIMEOUT: Duration = Duration::from_secs(10);
const PARK_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const FOCUS_GRACE: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Offered(MonoTime),
    Parking(MonoTime),
    Capturing(MonoTime),
    Live,
    Suspended(MonoTime),
    Resuming(MonoTime),
    Restarting(MonoTime),
}

/// What the destination reported about where its proxy's content is (WP-2.43 §4 "placement
/// state"). The high-water mark never decreases and survives a suspension; validity does not.
#[derive(Clone, Copy, Debug, Default)]
struct PlacementState {
    hwm: Option<u32>,
    last: Option<(u32, Option<DisplayId>, PointDevice, PixelSize)>,
    /// The newest accepted report belongs to this connection: set by an accepted report, cleared
    /// by a suspension (the destination re-sends its newest report after `Accepted`).
    valid: bool,
}

impl PlacementState {
    /// A `ProxyPlaced` is accepted when its generation is higher than the high-water mark, or
    /// equal with identical contents (a resend). `u32::MAX` is the terminal invalidation: it never
    /// carries a display, and no later report can exceed it.
    fn accept(
        &mut self,
        generation: u32,
        display: Option<DisplayId>,
        origin: PointDevice,
        size: PixelSize,
    ) {
        if !(origin.x.is_finite() && origin.y.is_finite()) {
            return;
        }
        let display = display.filter(|_| generation != u32::MAX);
        let report = (generation, display, origin, size);
        if self.hwm.is_none_or(|hwm| generation > hwm) {
            self.hwm = Some(generation);
            self.last = Some(report);
            self.valid = true;
        } else if self.hwm == Some(generation) && self.last == Some(report) {
            self.valid = true;
        }
    }

    /// The report as a coherent placement of content `content` pixels large, if there is one.
    fn placed(&self, content: PixelSize) -> Option<Placement> {
        let (generation, display, origin, size) = self.last.filter(|_| self.valid)?;
        let display = display.filter(|_| size == content)?;
        Some(Placement {
            generation,
            display,
            origin,
            size,
        })
    }
}

pub(super) struct Source {
    pub peer: NodeId,
    pub window: WindowId,
    restore_place: Option<ProxyPlacement>,
    stage: Stage,
    placement: PlacementState,
    parked: Option<Parked>,
    stream: Option<StreamId>,
    capture_pending: bool,
    resizing: bool,
    resume_geometry: Option<(PixelSize, f64)>,
    /// The newest resize request received but not started (latest value wins), with its number.
    latest_resize: Option<(PixelSize, f64, u32)>,
    /// The newest `Resize` request number received (0 before any): a request that isn't newer is
    /// a duplicate or arrived out of order.
    received: u32,
    /// The newest request number this projection has finished processing (0 before any): its
    /// `Geometry` says so, so the destination knows which of its requests the geometry reflects.
    answered: u32,
    /// The request number of the resize in flight (`resizing`); `None` if it isn't a request (a
    /// re-park, or a resume's). A destination that predates numbers sends 0.
    inflight: Option<u32>,
    /// The size the destination last asked for (Accepted or Resize).
    wanted: Option<PixelSize>,
    /// The window's frame when the last park or resize finished. (Not when it was issued: parking
    /// itself moves the window, so the frame before it says nothing about the one after.)
    parked_frame: Option<RectLogical>,
    /// The window's state when the last park or resize was *issued* (WP-2.45b), which is what
    /// that park was based on: a different state at its end, or later, means the window went
    /// fullscreen, was hidden, or came back meanwhile, and the parked geometry no longer
    /// describes it.
    parked_state: Option<WindowState>,
    /// The window's state as of the newest window event, so a park can record what it is based
    /// on when it is issued.
    window_state: WindowState,
    /// The window differed from what the last park was based on when it was last looked at, and
    /// a re-park could not follow at once (a park in flight, or `REPARK_GAP` not yet passed): look
    /// again, and re-park if it still does, at `last_repark + REPARK_GAP`.
    repark_due: bool,
    /// When the window last changed by itself and was parked again.
    last_repark: Option<MonoTime>,
    last_seq: u32,
    parked_scale: f64,
    last_activation: Option<MonoTime>,
    /// The destination's proxy has focus but the projection isn't live yet: activate the window
    /// when it is.
    focus_wanted: bool,
    pub(super) clipboard_focused: bool,
}

impl E2 {
    fn project_refusal(&self, window: WindowId, peer: NodeId) -> Option<Refusal> {
        if !self.granted(peer, Capability::WindowShare) {
            Some(Refusal::Permission)
        } else if !self.permits_io() {
            Some(Refusal::Locked)
        } else if !self.windows.contains_key(&window)
            || !self.peers.contains(&peer)
            || self.sources.values().any(|s| s.window == window)
            || self.pending_parks.contains_key(&window)
            || self.next_projection.is_none()
            || !self.ledgers.recovery_done()
        {
            Some(Refusal::Busy)
        } else {
            None
        }
    }

    pub(crate) fn drag_offer(
        &self,
        window: WindowId,
        peer: NodeId,
    ) -> Option<crate::e1::drag::Offer> {
        use crate::e1::drag::{Kind, Offer};
        if let Some(key) = self.proxy_windows.get(&window) {
            let (size, scale) = self.destinations.get(key)?.drag_geometry()?;
            return (key.source == peer && self.permits_io()).then_some(Offer {
                window,
                kind: Kind::Back(*key),
                peer,
                size,
                scale,
            });
        }
        self.project_refusal(window, peer).is_none().then_some(())?;
        let (_, size, scale) = self.window_details(self.windows.get(&window)?);
        Some(Offer {
            window,
            kind: Kind::Out(window),
            peer,
            size,
            scale,
        })
    }

    pub(crate) fn drag_title(&self, window: WindowId) -> String {
        if let Some(key) = self.proxy_windows.get(&window)
            && let Some(destination) = self.destinations.get(key)
        {
            return destination.title.clone();
        }
        self.windows
            .get(&window)
            .map(|info| info.title.clone())
            .unwrap_or_else(|| "window".into())
    }

    pub(crate) fn drag_commit(
        &mut self,
        commit: crate::e1::drag::Commit,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> Option<ProjectionKey> {
        match commit.kind {
            crate::e1::drag::Kind::Out(window) => {
                self.project(window, commit.peer, now, out).ok()?;
                self.place_start(
                    commit.place,
                    commit.token,
                    commit.anchor,
                    Some(commit.size),
                    out,
                )
            }
            crate::e1::drag::Kind::Back(key) => {
                self.return_at(key, commit.place, now, out);
                None
            }
        }
    }

    pub(super) fn place_start(
        &self,
        place: ProxyPlacement,
        token: u32,
        anchor: (i32, i32),
        placed_size: Option<PixelSize>,
        out: &mut [Output],
    ) -> Option<ProjectionKey> {
        let Output::SendControl {
            msg: crosspane_protocol::msg::ControlMessage::Projection(msg),
            ..
        } = out.last_mut()?
        else {
            return None;
        };
        let Message::Start {
            projection,
            window,
            size,
        } = msg
        else {
            return None;
        };
        let key = ProjectionKey {
            source: self.node,
            projection: *projection,
        };
        *msg = Message::StartAt {
            projection: *projection,
            window: window.clone(),
            size: placed_size.unwrap_or(*size),
            place,
            token,
            anchor,
        };
        Some(key)
    }

    pub(super) fn return_at(
        &mut self,
        key: ProjectionKey,
        place: ProxyPlacement,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if place.drag {
            return;
        }
        if key.source == self.node {
            if let Some(source) = self.sources.get_mut(&key.projection) {
                source.restore_place = Some(place);
            }
            self.end_source(key.projection, Reason::Returned, false, now, out);
        } else if self.destinations.contains_key(&key) {
            send(
                key.source,
                Message::ReturnAt {
                    projection: key.projection,
                    place,
                },
                out,
            );
            self.end_destination(key, Reason::Returned, true, false, out);
        }
    }

    pub(super) fn project(
        &mut self,
        window: WindowId,
        peer: NodeId,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> Result<(), Refusal> {
        let refusal = self.project_refusal(window, peer);
        if let Some(reason) = refusal {
            out.push(Output::Notice(Notice::ProjectionRefused { peer, reason }));
            return Err(reason);
        }
        let (Some(info), Some(id)) = (self.windows.get(&window), self.next_projection) else {
            out.push(Output::Notice(Notice::ProjectionRefused {
                peer,
                reason: Refusal::Busy,
            }));
            return Err(Refusal::Busy);
        };
        self.next_projection = id.checked_add(1);
        let projection = ProjectionId(id);
        if self.ledgers.open(projection).is_err() {
            out.push(Output::Notice(Notice::ProjectionRefused {
                peer,
                reason: Refusal::InjectorFailed,
            }));
            return Err(Refusal::InjectorFailed);
        }
        let (window_summary, size, scale) = self.window_details(info);
        send(
            peer,
            Message::Start {
                projection,
                window: window_summary,
                size,
            },
            out,
        );
        self.sources.insert(
            projection,
            Source {
                peer,
                window,
                restore_place: None,
                stage: Stage::Offered(now.saturating_add(START_TIMEOUT)),
                placement: PlacementState::default(),
                parked: None,
                stream: None,
                capture_pending: false,
                resizing: false,
                resume_geometry: None,
                latest_resize: None,
                received: 0,
                answered: 0,
                inflight: None,
                wanted: None,
                parked_frame: None,
                parked_state: None,
                window_state: info.state,
                repark_due: false,
                last_repark: None,
                last_seq: 0,
                parked_scale: scale,
                last_activation: None,
                focus_wanted: false,
                clipboard_focused: false,
            },
        );
        Ok(())
    }

    fn window_details(&self, info: &WindowInfo) -> (WindowSummary, PixelSize, f64) {
        let scale = info
            .display
            .and_then(|d| self.scales.get(&d))
            .copied()
            .filter(|s| s.is_finite() && *s > 0.0)
            .unwrap_or(1.0);
        let size = PixelSize::new(
            dimension(info.frame.size.width * scale),
            dimension(info.frame.size.height * scale),
        );
        (
            WindowSummary {
                title: wire_text(&info.title),
                app_id: wire_text(&info.app_id),
            },
            size,
            scale,
        )
    }

    pub(super) fn suspend_source(
        &mut self,
        projection: ProjectionId,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let Some(source) = self.sources.get_mut(&projection) else {
            return;
        };
        if matches!(source.stage, Stage::Offered(_)) {
            self.end_source(projection, Reason::LinkLost, true, now, out);
            return;
        }
        self.ledgers.retire(projection, now, out);
        if let Some(stream) = source.stream.take() {
            out.push(Output::StopCapture { stream });
        }
        // Keep bookkeeping for an operation already submitted, but discard queued resizes.
        // A late Parked result updates the parked geometry without restoring or capturing.
        source.resizing |= matches!(source.stage, Stage::Parking(_));
        source.latest_resize = None;
        source.resume_geometry = None;
        // Looked at again when the projection is live again.
        source.repark_due = false;
        // The placement was reported on the connection that just ended; the destination re-sends
        // it after `Accepted` (the high-water mark stays, so an older report never revives it).
        source.placement.valid = false;
        source.stage = Stage::Suspended(now.saturating_add(GRACE));
        self.drain_targeting_queue(now, out);
    }

    pub(super) fn resume_sources(&mut self, peer: NodeId, now: MonoTime, out: &mut Vec<Output>) {
        let ids: Vec<_> = self
            .sources
            .iter()
            .filter(|(_, s)| s.peer == peer && matches!(s.stage, Stage::Suspended(_)))
            .map(|(&id, _)| id)
            .collect();
        for projection in ids {
            let Some(source) = self.sources.get(&projection) else {
                continue;
            };
            if source.deadline().is_some_and(|deadline| deadline <= now) {
                self.end_source(projection, Reason::LinkLost, true, now, out);
                continue;
            }
            let Some(info) = self.windows.get(&source.window) else {
                self.end_source(projection, Reason::WindowClosed, true, now, out);
                continue;
            };
            let (window, size, _) = self.window_details(info);
            if self.ledgers.open(projection).is_err() {
                self.end_source(projection, Reason::LinkLost, false, now, out);
                continue;
            }
            if let Some(source) = self.sources.get_mut(&projection) {
                source.stage = Stage::Resuming(now.saturating_add(START_TIMEOUT));
            }
            send(
                peer,
                Message::Start {
                    projection,
                    window,
                    size,
                },
                out,
            );
        }
    }

    pub(super) fn source_control(
        &mut self,
        peer: NodeId,
        msg: &Message,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if let Message::ListWindows { request } | Message::Pull { request, .. } = msg {
            let refusal = if !self.granted(peer, Capability::WindowBrowse)
                || !self.granted(peer, Capability::WindowShare)
            {
                Some(Refusal::Permission)
            } else if !self.permits_io() {
                Some(Refusal::Locked)
            } else {
                None
            };
            if let Some(reason) = refusal {
                send(
                    peer,
                    Message::BrowseRefused {
                        request: *request,
                        reason,
                    },
                    out,
                );
                return;
            }
            if let Message::Pull { window, .. } = msg {
                if let Err(reason) = self.project(*window, peer, now, out) {
                    send(
                        peer,
                        Message::BrowseRefused {
                            request: *request,
                            reason,
                        },
                        out,
                    );
                }
            } else {
                // BTreeMap iteration supplies ascending WindowId order before the cap.
                let windows = self
                    .windows
                    .values()
                    .filter(|info| {
                        matches!(info.role, WindowRole::Toplevel | WindowRole::Dialog)
                            // Off-screen or off-Space: not something to pull (WP-2.45b).
                            && info.state != WindowState::Hidden
                            && !self.sources.values().any(|s| s.window == info.id)
                            && !self.pending_parks.contains_key(&info.id)
                    })
                    .take(MAX_BROWSE_WINDOWS)
                    .map(|info| {
                        let (summary, size, _) = self.window_details(info);
                        BrowsableWindow {
                            window: info.id,
                            summary,
                            size,
                        }
                    })
                    .collect();
                send(
                    peer,
                    Message::WindowList {
                        request: *request,
                        windows,
                    },
                    out,
                );
            }
            return;
        }
        let projection = match msg {
            Message::Accepted { projection, .. }
            | Message::Refused { projection, .. }
            | Message::Resize { projection, .. }
            | Message::Focus { projection, .. }
            | Message::KeyFrameRequest { projection }
            | Message::Close { projection, .. }
            | Message::ProxyPlaced { projection, .. } => *projection,
            _ => return,
        };
        let Some(source) = self.sources.get_mut(&projection).filter(|s| s.peer == peer) else {
            return;
        };
        match msg {
            Message::Accepted { size, scale, .. } if matches!(source.stage, Stage::Resuming(_)) => {
                if !sane_size(*size) {
                    self.end_source(projection, Reason::LinkLost, false, now, out);
                    return;
                }
                source.stage = Stage::Restarting(now.saturating_add(START_TIMEOUT));
                source.wanted = Some(*size);
                source.resume_geometry = Some((*size, *scale));
                if !source.restart(projection, now, out) {
                    self.end_source(projection, Reason::Failed, false, now, out);
                }
            }
            Message::Refused { .. } if matches!(source.stage, Stage::Resuming(_)) => {
                self.end_source(projection, Reason::LinkLost, true, now, out);
            }
            Message::Accepted { size, scale, .. } if matches!(source.stage, Stage::Offered(_)) => {
                if !sane_size(*size) {
                    self.end_source(projection, Reason::Failed, false, now, out);
                    return;
                }
                source.stage = Stage::Parking(now.saturating_add(START_TIMEOUT));
                source.parked_scale = *scale;
                source.wanted = Some(*size);
                source.mark_issued();
                out.push(Output::Park {
                    window: source.window,
                    size: *size,
                    scale: *scale,
                });
            }
            Message::Refused { reason, .. } if matches!(source.stage, Stage::Offered(_)) => {
                out.push(Output::Notice(Notice::ProjectionRefused {
                    peer,
                    reason: *reason,
                }));
                self.end_source(projection, Reason::Failed, true, now, out);
            }
            Message::Resize {
                request,
                size,
                scale,
                ..
            } => source.on_resize(projection, *request, *size, *scale, out),
            // Where the proxy's content is: kept in every stage the projection can have a proxy
            // on this connection, live or not (WP-2.43 §4).
            Message::ProxyPlaced {
                generation,
                display,
                origin,
                size,
                ..
            } if !matches!(source.stage, Stage::Suspended(_) | Stage::Resuming(_)) => {
                source
                    .placement
                    .accept(*generation, *display, *origin, *size);
            }
            // While this node's controller is home, the seat belongs to native input alone: no
            // focus request, restore or wish is acted on (WP-2.43 §2.4).
            Message::Focus { .. } if self.home.is_some() => {}
            Message::Focus { focused: true, .. } if source.stage == Stage::Live => {
                self.focus_source(projection, now, out);
            }
            Message::Focus { focused, .. }
                if matches!(source.stage, Stage::Parking(_) | Stage::Capturing(_)) =>
            {
                // A proxy usually has focus as soon as it opens, before the projection is live.
                source.focus_wanted = *focused;
            }
            Message::Focus { focused: false, .. } if source.stage == Stage::Live => {
                source.clipboard_focused = false;
                if let Some(window) = self.focus_before.take()
                    && self.windows.contains_key(&window)
                    && !self.sources.values().any(|s| s.window == window)
                {
                    out.push(Output::ActivateWindow { window });
                }
            }
            Message::KeyFrameRequest { .. } if source.stage == Stage::Live => {
                out.push(Output::RequestKeyFrame { projection });
            }
            Message::Close { reason, .. } => self.end_source(projection, *reason, true, now, out),
            _ => {}
        }
    }

    pub(super) fn parked(
        &mut self,
        window: WindowId,
        result: Result<Parked, Failure>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if self.pending_parks.remove(&window).is_some() {
            out.push(Output::Restore {
                window,
                place: self.pending_places.remove(&window),
            });
            return;
        }
        let current = self.windows.get(&window).map(|w| (w.frame, w.state));
        let frame = current.map(|(frame, _)| frame);
        let Some((&projection, source)) = self.sources.iter_mut().find(|(_, s)| {
            s.window == window && (matches!(s.stage, Stage::Parking(_)) || s.resizing)
        }) else {
            return;
        };
        let initial = matches!(source.stage, Stage::Parking(_));
        // This operation has answered, so ending it must not wait for another Parked result.
        source.resizing = false;
        // Its request, if it was one, is answered by the geometry below; a newer queued one is
        // not (it has its own operation).
        if let Some(request) = source.inflight.take() {
            source.answered = request;
        }
        if initial {
            source.stage = Stage::Capturing(now.saturating_add(START_TIMEOUT));
        }
        let Ok(parked) = result else {
            self.end_source(projection, Reason::Failed, false, now, out);
            return;
        };
        let Some((size, parking)) = geometry(parked).filter(|_| parked.window == window) else {
            self.end_source(projection, Reason::Failed, false, now, out);
            return;
        };
        source.parked = Some(parked);
        source.parked_frame = frame;
        if initial {
            source.start_capture(projection, parked, size, parking, now, out);
            out.push(Output::Notice(Notice::ProjectionStarted {
                key: ProjectionKey {
                    source: self.node,
                    projection,
                },
                peer: source.peer,
                parking,
            }));
        } else if matches!(source.stage, Stage::Restarting(_)) {
            if !source.restart(projection, now, out) {
                self.end_source(projection, Reason::Failed, false, now, out);
            }
        } else if !matches!(source.stage, Stage::Suspended(_) | Stage::Resuming(_)) {
            if parking == ParkingKind::Twin
                && let Some(stream) = source.stream
            {
                out.push(Output::SetCaptureCrop {
                    stream,
                    crop: Some(parked.content),
                });
            }
            send(
                source.peer,
                Message::Geometry {
                    projection,
                    size,
                    parking,
                    answers: source.answered,
                },
                out,
            );
            source.resizing = false;
            source.resize_latest(projection, out);
            // The window may have changed state while this park ran (the app went fullscreen,
            // or the park itself un-fullscreened it): then the geometry just sent is stale.
            // Nothing more is coming to say so, so look now (and again at the gap's end).
            if let Some((frame, state)) = current {
                source.check_repark(frame, state, now, out);
            }
        }
    }

    pub(super) fn capture_started(
        &mut self,
        projection: ProjectionId,
        result: Result<StreamId, Failure>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if let Some(source) = self
            .sources
            .get_mut(&projection)
            .filter(|s| s.capture_pending)
        {
            source.capture_pending = false;
            if !matches!(source.stage, Stage::Capturing(_)) {
                if let Ok(stream) = result {
                    out.push(Output::StopCapture { stream });
                }
                if matches!(source.stage, Stage::Restarting(_))
                    && !source.restart(projection, now, out)
                {
                    self.end_source(projection, Reason::Failed, false, now, out);
                }
                return;
            }
            match result {
                Ok(stream) => {
                    source.stream = Some(stream);
                    source.stage = Stage::Live;
                    source.resize_latest(projection, out);
                    // Window changes were not acted on before the projection was live: look at
                    // the window as it is now against what the park was based on.
                    if let Some(w) = self.windows.get(&source.window) {
                        source.check_repark(w.frame, w.state, now, out);
                    }
                    if source.focus_wanted {
                        if self.home.is_none() {
                            self.focus_source(projection, now, out);
                        } else {
                            // The seat is arbitrated (WP-2.43 §2.4): the wish is dropped.
                            source.focus_wanted = false;
                        }
                    }
                }
                Err(_) => self.end_source(projection, Reason::Failed, false, now, out),
            }
        } else if let Ok(stream) = result
            && !self.sources.values().any(|s| s.stream == Some(stream))
        {
            // A start can complete after lock/return: immediately stop the orphaned stream.
            out.push(Output::StopCapture { stream });
        }
    }

    pub(super) fn capture_ended(
        &mut self,
        stream: StreamId,
        reason: StreamEndReason,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let reason = match reason {
            StreamEndReason::Requested => return,
            StreamEndReason::Blocked => Reason::Locked,
            StreamEndReason::TargetGone => Reason::WindowClosed,
            _ => Reason::Failed,
        };
        if let Some((&projection, _)) = self.sources.iter().find(|(_, s)| s.stream == Some(stream))
        {
            self.end_source(projection, reason, false, now, out);
        }
    }

    pub(super) fn source_input(
        &mut self,
        peer: NodeId,
        msg: &ProjInput,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if !self.permits_io() || !self.ledgers.recovery_done() {
            return;
        }
        let (projection, seq) = match msg {
            ProjInput::Key {
                projection, seq, ..
            }
            | ProjInput::Button {
                projection, seq, ..
            }
            | ProjInput::Scroll {
                projection, seq, ..
            }
            | ProjInput::Motion {
                projection, seq, ..
            }
            | ProjInput::Held {
                projection, seq, ..
            } => (*projection, *seq),
            _ => return,
        };
        let Some(source) = self
            .sources
            .get_mut(&projection)
            .filter(|s| s.peer == peer && s.stage == Stage::Live && seq > s.last_seq)
        else {
            return;
        };
        source.last_seq = seq;
        // While this node's controller is home (WP-2.43 §2.4) every input of every source only
        // advances `last_seq`: nothing reaches a ledger and no `MoveTo` is issued, so no
        // Crosspane-injected key can reach the home bind, and the drain's releases (the only
        // injections the filter admits) are the last this node's injectors saw.
        if self.home.is_some() {
            return;
        }
        self.source_ready_input(projection, msg, now, out);
    }

    /// Process inputs whose sequence numbers were already accepted, including a targeting FIFO.
    fn source_ready_input(
        &mut self,
        projection: ProjectionId,
        msg: &ProjInput,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if self.home.is_some() || !self.permits_io() || !self.ledgers.recovery_done() {
            return;
        }
        let Some(source) = self
            .sources
            .get_mut(&projection)
            .filter(|s| s.stage == Stage::Live)
        else {
            return;
        };
        // A batch teardown can drain the FIFO before another affected source is retired.
        if !self.peers.contains(&source.peer)
            || !self
                .grants
                .get(&source.peer)
                .is_some_and(|grants| grants.contains(&Capability::WindowShare))
        {
            return;
        }
        // Held reports still refresh the lease and release missing items immediately.
        if !matches!(msg, ProjInput::Held { .. }) {
            match self.ledgers.queue_targeted(projection, msg) {
                Ok(true) => return,
                Ok(false) => {}
                Err(()) => {
                    self.end_source(projection, Reason::Failed, false, now, out);
                    return;
                }
            }
        }
        let Some(parked) = source.parked else { return };
        let mut item = None;
        match msg {
            ProjInput::Motion { position, .. } => {
                self.ledgers.inject(move_to(parked, *position), out)
            }
            ProjInput::Button {
                button,
                down,
                position,
                ..
            } => {
                if *down {
                    self.ledgers.target(
                        projection,
                        move_to(parked, *position),
                        msg.clone(),
                        now,
                        out,
                    );
                    return;
                }
                // A rejected or cancelled press never entered the ledger and owes no release.
                if !self.ledgers.holds(projection, Held::Button(*button)) {
                    return;
                }
                self.ledgers.inject(move_to(parked, *position), out);
                item = Some((Held::Button(*button), *down));
            }
            ProjInput::Scroll { position, .. } => {
                self.ledgers.target(
                    projection,
                    move_to(parked, *position),
                    msg.clone(),
                    now,
                    out,
                );
            }
            ProjInput::Key { usage, down, .. } => {
                if *down && !focus_on(&self.windows, self.focused, source.window, parked.display) {
                    if source.activate(now, out) {
                        self.focused = None;
                    }
                    // Focus is confirmed only by WindowEvent::Focused, never by elapsed time.
                    return;
                }
                item = Some((Held::Key(*usage), *down));
            }
            ProjInput::Held { keys, buttons, .. } => {
                let items: Vec<_> = keys
                    .iter()
                    .copied()
                    .map(Held::Key)
                    .chain(buttons.iter().copied().map(Held::Button))
                    .collect();
                self.ledgers.input_heartbeat(projection, &items, now, out);
            }
            _ => {}
        }
        if let Some((item, down)) = item
            && !self.ledgers.input(projection, item, down, now, out)
        {
            self.end_source(projection, Reason::Failed, false, now, out);
        }
    }

    pub(super) fn source_inject_done(
        &mut self,
        id: InjectId,
        ok: bool,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        // A late callback must not let lease cleanup erase an overdue targeting failure.
        if let Some(owner) = self.ledgers.targeting_expired(now) {
            self.end_source(owner, Reason::Failed, false, now, out);
        }
        self.ledgers.expire_leases(now, out);
        let Some(targeting) = self.ledgers.targeted(id) else {
            self.drain_targeting_queue(now, out);
            return;
        };
        let projection = targeting.owner;
        if self.home.is_some()
            || !self.permits_io()
            || !self
                .sources
                .get(&projection)
                .is_some_and(|s| s.stage == Stage::Live)
        {
            return;
        }
        if ok {
            match targeting.input {
                ProjInput::Button {
                    button, down: true, ..
                } => {
                    if !self
                        .ledgers
                        .input(projection, Held::Button(button), true, now, out)
                    {
                        self.end_source(projection, Reason::Failed, false, now, out);
                        return;
                    }
                }
                ProjInput::Scroll { delta, .. } => {
                    self.ledgers.inject(InjectCmd::Scroll(delta), out)
                }
                _ => {}
            }
        }
        self.drain_targeting_queue(now, out);
    }

    fn drain_targeting_queue(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        if let Some(owner) = self.ledgers.targeting_expired(now) {
            self.end_source(owner, Reason::Failed, false, now, out);
        }
        // Every replay path, including teardown, discards expired entries before they renew leases.
        self.ledgers.expire_leases(now, out);
        while let Some((projection, input)) = self.ledgers.next_targeted_input() {
            self.source_ready_input(projection, &input, now, out);
        }
    }

    /// WP-2.43 §2.3: the content position of a `ProjInput::Motion` from `peer` that passes every
    /// E2 check (the gate, recovery, the peer, a live twin source, `seq > last_seq`, the
    /// `WindowShare` grant, and a finite position inside `[0, content.size())`). Pure: nothing is
    /// recorded, so the same motion is then processed by [`E2::source_input`] as usual.
    pub(crate) fn prevalidate_motion(
        &self,
        peer: NodeId,
        msg: &ProjInput,
    ) -> Option<(ProjectionId, PointDevice)> {
        let ProjInput::Motion {
            projection,
            seq,
            position,
        } = msg
        else {
            return None;
        };
        if !self.permits_io() || !self.ledgers.recovery_done() {
            return None;
        }
        let source = self.sources.get(projection)?;
        if source.peer != peer
            || source.stage != Stage::Live
            || *seq <= source.last_seq
            || !self.granted(peer, Capability::WindowShare)
        {
            return None;
        }
        let (size, kind) = geometry(source.parked?)?;
        if kind != ParkingKind::Twin
            || !position.x.is_finite()
            || !position.y.is_finite()
            || position.x < 0.0
            || position.y < 0.0
            || position.x >= f64::from(size.width)
            || position.y >= f64::from(size.height)
        {
            return None;
        }
        Some((*projection, *position))
    }

    /// WP-2.43 §4: every live twin-parked source, in `ProjectionId` order.
    pub(crate) fn twin_homes(&self) -> Vec<TwinHome> {
        self.sources
            .iter()
            .filter_map(|(&projection, source)| {
                if source.stage != Stage::Live {
                    return None;
                }
                let parked = source.parked?;
                let (size, kind) = geometry(parked)?;
                if kind != ParkingKind::Twin {
                    return None;
                }
                Some(TwinHome {
                    peer: source.peer,
                    projection,
                    window: source.window,
                    display: parked.display,
                    content: parked.content,
                    placed: source.placement.placed(size),
                    focused: focus_on(&self.windows, self.focused, source.window, parked.display),
                })
            })
            .collect()
    }

    /// The destination focused the proxy: bring the source window forward, unless the OS already
    /// reports it focused. Window sources report focus only when it changes, so asking again and
    /// waiting for a confirmation would wait forever (and hold back every key).
    fn focus_source(&mut self, projection: ProjectionId, now: MonoTime, out: &mut Vec<Output>) {
        let focused = self.focused;
        let Some(source) = self.sources.get_mut(&projection) else {
            return;
        };
        // Only reached by accepted live focus, including a deferred wish committed at capture.
        source.clipboard_focused = true;
        source.focus_wanted = false;
        if source
            .parked
            .is_some_and(|p| focus_on(&self.windows, focused, source.window, p.display))
        {
            return;
        }
        if source.activate(now, out) {
            if self.focus_before.is_none()
                && let Some(previous) = focused
                && !self.sources.values().any(|s| s.window == previous)
            {
                self.focus_before = Some(previous);
            }
            self.focused = None;
        }
    }

    pub(super) fn end_source(
        &mut self,
        projection: ProjectionId,
        reason: Reason,
        peer_ended: bool,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let Some(source) = self.sources.remove(&projection) else {
            return;
        };
        self.ledgers.retire(projection, now, out);
        if let Some(stream) = source.stream {
            out.push(Output::StopCapture { stream });
        }
        if !matches!(source.stage, Stage::Offered(_)) || source.restore_place.is_some() {
            out.push(Output::Restore {
                window: source.window,
                place: source.restore_place,
            });
        }
        if matches!(source.stage, Stage::Parking(_)) || source.resizing {
            self.pending_parks
                .insert(source.window, now.saturating_add(PARK_CLEANUP_TIMEOUT));
            if let Some(place) = source.restore_place {
                self.pending_places.insert(source.window, place);
            }
        }
        if !peer_ended
            && !matches!(source.stage, Stage::Suspended(_))
            && self.peers.contains(&source.peer)
        {
            send(source.peer, Message::End { projection, reason }, out);
        }
        out.push(Output::Notice(Notice::ProjectionEnded {
            key: ProjectionKey {
                source: self.node,
                projection,
            },
            reason,
        }));
        self.drain_targeting_queue(now, out);
    }

    pub(super) fn source_tick(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        self.drain_targeting_queue(now, out);
        let expired: Vec<_> = self
            .sources
            .iter()
            .filter(|(_, s)| s.deadline().is_some_and(|deadline| deadline <= now))
            .map(|(&id, s)| (id, s.peer, s.stage))
            .collect();
        for (id, peer, stage) in expired {
            if matches!(stage, Stage::Offered(_)) {
                out.push(Output::Notice(Notice::ProjectionRefused {
                    peer,
                    reason: Refusal::Busy,
                }));
            }
            let reason = if matches!(stage, Stage::Suspended(_) | Stage::Resuming(_)) {
                Reason::LinkLost
            } else {
                Reason::Failed
            };
            self.end_source(id, reason, matches!(stage, Stage::Suspended(_)), now, out);
        }
        let windows: Vec<_> = self
            .pending_parks
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(&window, _)| window)
            .collect();
        for window in windows {
            self.pending_parks.remove(&window);
            out.push(Output::Restore {
                window,
                place: self.pending_places.remove(&window),
            });
        }
        // Re-parks that a change had to wait for (a park in flight, or the gap): the gap has
        // ended, so look at the window again. Whatever it finds, the flag is spent.
        let due: Vec<_> = self
            .sources
            .iter()
            .filter(|(_, s)| s.repark_deadline().is_some_and(|deadline| deadline <= now))
            .map(|(&id, _)| id)
            .collect();
        for id in due {
            let Some(source) = self.sources.get_mut(&id) else {
                continue;
            };
            match self.windows.get(&source.window) {
                Some(w) => source.check_repark(w.frame, w.state, now, out),
                None => source.repark_due = false,
            }
        }
    }

    pub(super) fn source_deadline(&self) -> Option<MonoTime> {
        self.sources
            .values()
            .filter_map(Source::deadline)
            .chain(self.sources.values().filter_map(Source::repark_deadline))
            .chain(self.pending_parks.values().copied())
            .min()
    }

    pub(super) fn window_changed(
        &mut self,
        window: &WindowInfo,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        // A parked window that moved or resized by itself (e.g. a bar appeared on, or left, its
        // twin display and changed the work area), or whose state changed (it went fullscreen,
        // was hidden, or came back), gets parked again at the wanted size, so the capture crop
        // and the destination's geometry follow it. Compared with its frame when the last park
        // finished and its state when it was issued, and at most every REPARK_GAP: parking
        // itself moves the window for a moment, and that must not start a loop. A change that
        // can't be followed yet (a park in flight, or the gap) is not lost: it is looked at again
        // when the park finishes and when the gap ends.
        for source in self.sources.values_mut() {
            if source.window == window.id {
                source.window_state = window.state;
                source.check_repark(window.frame, window.state, now, out);
            }
        }
        if self
            .windows
            .get(&window.id)
            .is_some_and(|old| old.title != window.title)
        {
            for (&projection, source) in &self.sources {
                if source.window == window.id && !matches!(source.stage, Stage::Suspended(_)) {
                    send(
                        source.peer,
                        Message::Title {
                            projection,
                            title: wire_text(&window.title),
                        },
                        out,
                    );
                }
            }
        }
    }
}

impl Source {
    /// Whether a window with this `frame` and `state` no longer matches what the last park was
    /// based on: its state differs, or it moved while that state was `Normal`. A frame means
    /// nothing while the window is fullscreen or hidden (the platform reports the display's, or
    /// an off-screen one), so a move then is not a trigger; the state change out of it is.
    fn reparks_for(&self, frame: RectLogical, state: WindowState) -> bool {
        let state_changed = self.parked_state.is_some_and(|s| s != state);
        let moved = self.parked_state == Some(WindowState::Normal)
            && self.parked_frame.is_some_and(|f| !same_frame(f, frame));
        state_changed || moved
    }

    /// A park or resize is being issued: it is based on the window's state as it is now.
    fn mark_issued(&mut self) {
        self.parked_state = Some(self.window_state);
    }

    /// Park again if the window, as it is now (`frame`, `state`), no longer matches what the last
    /// park was based on, subject to `REPARK_GAP`. Only a live projection with nothing in
    /// flight can act on it. Otherwise:
    /// - in flight, or inside the gap: the change is remembered (`repark_due`) and looked at
    ///   again when the park finishes and at `last_repark + REPARK_GAP`;
    /// - not live yet (parking, capturing, restarting, suspended): nothing is remembered, since
    ///   the projection looks again when it goes live.
    fn check_repark(
        &mut self,
        frame: RectLogical,
        state: WindowState,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if self.stage != Stage::Live {
            return;
        }
        if !self.reparks_for(frame, state) {
            self.repark_due = false;
        } else if self.resizing || !self.gap_passed(now) {
            self.repark_due = true;
        } else {
            self.repark(now, out);
        }
    }

    /// `REPARK_GAP` has passed since the last re-park.
    fn gap_passed(&self, now: MonoTime) -> bool {
        self.last_repark
            .is_none_or(|t| now.saturating_duration_since(t) >= REPARK_GAP)
    }

    /// Park the window again at the wanted size, answering no request.
    fn repark(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        self.repark_due = false;
        let Some(size) = self.wanted else {
            return;
        };
        self.resizing = true;
        self.inflight = None;
        self.last_repark = Some(now);
        self.mark_issued();
        out.push(Output::ResizeParked {
            window: self.window,
            size,
            scale: self.parked_scale,
        });
    }

    /// When a re-park that had to wait can be tried: the gap's end. Not while a park is in
    /// flight (its result looks again) or the projection isn't live.
    fn repark_deadline(&self) -> Option<MonoTime> {
        if self.stage != Stage::Live || !self.repark_due || self.resizing {
            return None;
        }
        Some(
            self.last_repark
                .map_or(MonoTime::ZERO, |t| t.saturating_add(REPARK_GAP)),
        )
    }

    fn deadline(&self) -> Option<MonoTime> {
        match self.stage {
            Stage::Offered(deadline)
            | Stage::Parking(deadline)
            | Stage::Capturing(deadline)
            | Stage::Suspended(deadline)
            | Stage::Resuming(deadline)
            | Stage::Restarting(deadline) => Some(deadline),
            Stage::Live => None,
        }
    }

    /// Serialize restart behind old platform operations: CaptureStarted has only a projection
    /// id, so an old start must answer before another start is issued for that same id.
    fn restart(&mut self, projection: ProjectionId, now: MonoTime, out: &mut Vec<Output>) -> bool {
        if self.resizing || self.capture_pending {
            return true;
        }
        let Some(parked) = self.parked else {
            return false;
        };
        let Some((size, parking)) = geometry(parked) else {
            return false;
        };
        if let Some((wanted, scale)) = self.resume_geometry.take()
            && (size != wanted || self.parked_scale != scale)
        {
            self.resizing = true;
            self.inflight = None;
            self.parked_scale = scale;
            self.mark_issued();
            out.push(Output::ResizeParked {
                window: self.window,
                size: wanted,
                scale,
            });
            return true;
        }
        self.start_capture(projection, parked, size, parking, now, out);
        true
    }

    fn start_capture(
        &mut self,
        projection: ProjectionId,
        parked: Parked,
        size: PixelSize,
        parking: ParkingKind,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let (target, crop) = match parking {
            ParkingKind::Twin => (CaptureTarget::Display(parked.display), Some(parked.content)),
            _ => (CaptureTarget::Window(self.window), None),
        };
        self.stage = Stage::Capturing(now.saturating_add(START_TIMEOUT));
        self.capture_pending = true;
        out.push(Output::StartCapture {
            projection,
            peer: self.peer,
            target,
            crop,
            max_fps: 60,
        });
        send(
            self.peer,
            Message::Geometry {
                projection,
                size,
                parking,
                answers: self.answered,
            },
            out,
        );
    }

    fn activate(&mut self, now: MonoTime, out: &mut Vec<Output>) -> bool {
        if self
            .last_activation
            .is_some_and(|last| now.saturating_duration_since(last) < FOCUS_GRACE)
        {
            return false;
        }
        self.last_activation = Some(now);
        out.push(Output::ActivateWindow {
            window: self.window,
        });
        true
    }

    /// A `Resize` from the destination.
    fn on_resize(
        &mut self,
        projection: ProjectionId,
        request: u32,
        size: PixelSize,
        scale: f64,
        out: &mut Vec<Output>,
    ) {
        // Request numbers only grow. One that doesn't is a duplicate, or arrived out of order.
        // 0 is a destination that predates numbering: always the newest, answered with 0.
        if request != 0 && request <= self.received {
            return;
        }
        if matches!(self.stage, Stage::Suspended(_)) {
            return;
        }
        self.received = self.received.max(request);
        // A size outside the sane range is refused, but still a numbered request: it replaces
        // any queued one and is answered in its turn (with the actual size, no platform work),
        // so the destination never waits for an answer that can't come.
        let sane = sane_size(size);
        if sane {
            self.wanted = Some(size);
        }
        if self.stage != Stage::Live || self.resizing {
            // Not now: the newest request wins, and the older one is not answered on its own.
            self.latest_resize = Some((size, scale, request));
            return;
        }
        if !sane {
            self.answer(projection, request, out);
            return;
        }
        // Satisfied means the window really is this size at this scale. What was asked before
        // doesn't count: an app's minimum size makes the two differ.
        let satisfied = self.parked_scale == scale
            && self.parked.and_then(geometry).map(|(actual, _)| actual) == Some(size);
        if satisfied {
            self.answer(projection, request, out);
        } else {
            self.begin_resize(size, scale, request, out);
        }
    }

    fn begin_resize(&mut self, size: PixelSize, scale: f64, request: u32, out: &mut Vec<Output>) {
        self.resizing = true;
        self.parked_scale = scale;
        self.inflight = Some(request);
        self.mark_issued();
        out.push(Output::ResizeParked {
            window: self.window,
            size,
            scale,
        });
    }

    /// Answer `request` with the window's actual geometry, without a platform operation.
    fn answer(&mut self, projection: ProjectionId, request: u32, out: &mut Vec<Output>) {
        self.answered = request;
        if let Some((size, parking)) = self.parked.and_then(geometry) {
            send(
                self.peer,
                Message::Geometry {
                    projection,
                    size,
                    parking,
                    answers: self.answered,
                },
                out,
            );
        }
    }

    fn resize_latest(&mut self, projection: ProjectionId, out: &mut Vec<Output>) {
        let Some((size, scale, request)) = self.latest_resize.take() else {
            return;
        };
        let actual = self.parked.and_then(geometry).map(|(actual, _)| actual);
        if !sane_size(size) || (actual == Some(size) && self.parked_scale == scale) {
            self.answer(projection, request, out);
        } else {
            self.begin_resize(size, scale, request, out);
        }
    }
}

/// Whether the OS focus is on this projection: the projected window itself, or another window of
/// the same app on the display it is parked on. On macOS an app's popups, completion lists and
/// sheets are windows of their own (Safari's address-bar suggestions take focus as soon as they
/// appear), and nothing but the projection lives on a twin display. Keys never go to another app.
fn focus_on(
    windows: &BTreeMap<WindowId, WindowInfo>,
    focused: Option<WindowId>,
    window: WindowId,
    display: DisplayId,
) -> bool {
    let Some(focused) = focused else {
        return false;
    };
    if focused == window {
        return true;
    }
    match (windows.get(&focused), windows.get(&window)) {
        (Some(f), Some(w)) => f.pid.is_some() && f.pid == w.pid && f.display == Some(display),
        _ => false,
    }
}

/// The least time between two re-parks of a window that moved or changed state by itself.
const REPARK_GAP: Duration = Duration::from_secs(2);

/// Frames within half a logical pixel are the same (Hyprland reports whole pixels; float noise).
fn same_frame(a: RectLogical, b: RectLogical) -> bool {
    (a.origin.x - b.origin.x).abs() < 0.5
        && (a.origin.y - b.origin.y).abs() < 0.5
        && (a.size.width - b.size.width).abs() < 0.5
        && (a.size.height - b.size.height).abs() < 0.5
}

fn sane_size(size: PixelSize) -> bool {
    (1..=16384).contains(&size.width) && (1..=16384).contains(&size.height)
}

fn wire_text(value: &str) -> String {
    let mut end = value.len().min(1024);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn dimension(value: f64) -> u32 {
    if value.is_finite() {
        value.round().max(1.0) as u32
    } else {
        1
    }
}

fn geometry(parked: Parked) -> Option<(PixelSize, ParkingKind)> {
    let width = parked.content.max.x.checked_sub(parked.content.min.x)?;
    let height = parked.content.max.y.checked_sub(parked.content.min.y)?;
    if width <= 0 || height <= 0 {
        return None;
    }
    let parking = match parked.kind {
        PlatformParking::Twin => ParkingKind::Twin,
        PlatformParking::Mirror => ParkingKind::Mirror,
        _ => return None,
    };
    Some((PixelSize::new(width as u32, height as u32), parking))
}

fn move_to(parked: Parked, p: PointDevice) -> InjectCmd {
    let coordinate = |offset: i32, max: i32, value: f64| {
        let min = f64::from(offset);
        if value.is_finite() {
            (min + value).clamp(min, f64::from(max) - 1.0)
        } else {
            min
        }
    };
    InjectCmd::MoveTo {
        display: parked.display,
        position: PointDevice::new(
            coordinate(parked.content.min.x, parked.content.max.x, p.x),
            coordinate(parked.content.min.y, parked.content.max.y, p.y),
        ),
    }
}
