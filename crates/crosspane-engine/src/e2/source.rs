//! Lifecycle, focus guard and content-to-display input mapping on the window owner.

use std::time::Duration;

use crosspane_input::Held;
use crosspane_platform::{
    CaptureTarget, Parked, ParkingKind as PlatformParking, StreamEndReason, StreamId, WindowInfo,
    WindowRole,
};
use crosspane_protocol::msg::{Capability, Refusal};
use crosspane_protocol::projection::{
    BrowsableWindow, MAX_BROWSE_WINDOWS, ParkingKind, ProjInput, ProjectionEndReason as Reason,
    ProjectionMessage as Message, WindowSummary,
};
use crosspane_types::geom::{PixelSize, PointDevice, RectLogical};
use crosspane_types::id::{NodeId, ProjectionId, WindowId};
use crosspane_types::time::MonoTime;

use super::{E2, GRACE, send};
use crate::io::{Failure, InjectCmd, Notice, Output, ProjectionKey};

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

pub(super) struct Source {
    pub peer: NodeId,
    pub window: WindowId,
    stage: Stage,
    parked: Option<Parked>,
    stream: Option<StreamId>,
    capture_pending: bool,
    resizing: bool,
    resume_geometry: Option<(PixelSize, f64)>,
    latest_resize: Option<(PixelSize, f64)>,
    /// The size the destination last asked for (Accepted or Resize).
    wanted: Option<PixelSize>,
    /// The window's frame when the last park or resize finished.
    parked_frame: Option<RectLogical>,
    /// When the window last moved by itself and was parked again.
    last_repark: Option<MonoTime>,
    last_seq: u32,
    parked_scale: f64,
    last_activation: Option<MonoTime>,
}

impl E2 {
    pub(super) fn project(
        &mut self,
        window: WindowId,
        peer: NodeId,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> Result<(), Refusal> {
        let refusal = if !self.granted(peer, Capability::WindowShare) {
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
        };
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
                stage: Stage::Offered(now.saturating_add(START_TIMEOUT)),
                parked: None,
                stream: None,
                capture_pending: false,
                resizing: false,
                resume_geometry: None,
                latest_resize: None,
                wanted: None,
                parked_frame: None,
                last_repark: None,
                last_seq: 0,
                parked_scale: scale,
                last_activation: None,
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
        self.ledgers.retire(projection, out);
        if let Some(stream) = source.stream.take() {
            out.push(Output::StopCapture { stream });
        }
        // Keep bookkeeping for an operation already submitted, but discard queued resizes.
        // A late Parked result updates the parked geometry without restoring or capturing.
        source.resizing |= matches!(source.stage, Stage::Parking(_));
        source.latest_resize = None;
        source.resume_geometry = None;
        source.stage = Stage::Suspended(now.saturating_add(GRACE));
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
            | Message::Close { projection, .. } => *projection,
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
            Message::Resize { size, scale, .. }
                if source.stage == Stage::Live
                    && !source.resizing
                    && source.wanted == Some(*size)
                    && source.parked_scale == *scale =>
            {
                // Nothing to do: the window is already parked at this size and scale.
            }
            Message::Resize { size, scale, .. }
                if sane_size(*size) && !matches!(source.stage, Stage::Suspended(_)) =>
            {
                source.wanted = Some(*size);
                if source.stage != Stage::Live || source.resizing {
                    source.latest_resize = Some((*size, *scale));
                } else {
                    source.resizing = true;
                    source.parked_scale = *scale;
                    out.push(Output::ResizeParked {
                        window: source.window,
                        size: *size,
                        scale: *scale,
                    });
                }
            }
            Message::Focus { focused: true, .. } if source.stage == Stage::Live => {
                let previous = self.focused;
                if source.activate(now, out) {
                    if self.focus_before.is_none()
                        && let Some(previous) = previous
                        && !self.sources.values().any(|s| s.window == previous)
                    {
                        self.focus_before = Some(previous);
                    }
                    self.focused = None;
                }
            }
            Message::Focus { focused: false, .. } if source.stage == Stage::Live => {
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
            out.push(Output::Restore { window });
            return;
        }
        let frame = self.windows.get(&window).map(|w| w.frame);
        let Some((&projection, source)) = self.sources.iter_mut().find(|(_, s)| {
            s.window == window && (matches!(s.stage, Stage::Parking(_)) || s.resizing)
        }) else {
            return;
        };
        let initial = matches!(source.stage, Stage::Parking(_));
        // This operation has answered, so ending it must not wait for another Parked result.
        source.resizing = false;
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
                },
                out,
            );
            source.resizing = false;
            source.resize_latest(out);
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
                    source.resize_latest(out);
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
                self.ledgers.inject(move_to(parked, *position), out);
                item = Some((Held::Button(*button), *down));
            }
            ProjInput::Scroll {
                position, delta, ..
            } => {
                self.ledgers.inject(move_to(parked, *position), out);
                self.ledgers.inject(InjectCmd::Scroll(*delta), out);
            }
            ProjInput::Key { usage, down, .. } => {
                if *down && self.focused != Some(source.window) {
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
                self.ledgers.heartbeat(projection, &items, now, out);
            }
            _ => {}
        }
        if let Some((item, down)) = item
            && !self.ledgers.input(projection, item, down, now, out)
        {
            self.end_source(projection, Reason::Failed, false, now, out);
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
        self.ledgers.retire(projection, out);
        if let Some(stream) = source.stream {
            out.push(Output::StopCapture { stream });
        }
        if !matches!(source.stage, Stage::Offered(_)) {
            out.push(Output::Restore {
                window: source.window,
            });
        }
        if matches!(source.stage, Stage::Parking(_)) || source.resizing {
            self.pending_parks
                .insert(source.window, now.saturating_add(PARK_CLEANUP_TIMEOUT));
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
    }

    pub(super) fn source_tick(&mut self, now: MonoTime, out: &mut Vec<Output>) {
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
            out.push(Output::Restore { window });
        }
    }

    pub(super) fn source_deadline(&self) -> Option<MonoTime> {
        self.sources
            .values()
            .filter_map(Source::deadline)
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
        // twin display and changed the work area) gets parked again at the wanted size, so the
        // capture crop and the destination's geometry follow it. Compared with its frame when
        // the last park finished, and at most every REPARK_GAP: parking itself moves the window
        // for a moment, and that must not start a loop.
        for source in self.sources.values_mut() {
            let moved = source
                .parked_frame
                .is_some_and(|f| !same_frame(f, window.frame));
            if source.window == window.id
                && source.stage == Stage::Live
                && !source.resizing
                && moved
                && source
                    .last_repark
                    .is_none_or(|t| now.saturating_duration_since(t) >= REPARK_GAP)
                && let Some(size) = source.wanted
            {
                source.resizing = true;
                source.last_repark = Some(now);
                out.push(Output::ResizeParked {
                    window: source.window,
                    size,
                    scale: source.parked_scale,
                });
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
            self.parked_scale = scale;
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

    fn resize_latest(&mut self, out: &mut Vec<Output>) {
        if let Some((size, scale)) = self.latest_resize.take()
            && (self
                .parked
                .and_then(geometry)
                .is_none_or(|(actual, _)| actual != size)
                || self.parked_scale != scale)
        {
            self.resizing = true;
            self.parked_scale = scale;
            out.push(Output::ResizeParked {
                window: self.window,
                size,
                scale,
            });
        }
    }
}

/// The least time between two re-parks of a window that moved by itself.
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
