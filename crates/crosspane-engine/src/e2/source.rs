//! Lifecycle, focus guard and content-to-display input mapping on the window owner.

use std::time::Duration;

use crosspane_input::Held;
use crosspane_platform::{
    CaptureTarget, Parked, ParkingKind as PlatformParking, StreamEndReason, StreamId, WindowInfo,
};
use crosspane_protocol::msg::{Capability, Refusal};
use crosspane_protocol::projection::{
    ParkingKind, ProjInput, ProjectionEndReason as Reason, ProjectionMessage as Message,
    WindowSummary,
};
use crosspane_types::geom::{PixelSize, PointDevice};
use crosspane_types::id::{NodeId, ProjectionId, WindowId};
use crosspane_types::time::MonoTime;

use super::{E2, send};
use crate::io::{Failure, InjectCmd, Notice, Output, ProjectionKey};

const OFFER_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Offered(MonoTime),
    Parking,
    Capturing,
    Live,
}

pub(super) struct Source {
    pub peer: NodeId,
    pub window: WindowId,
    stage: Stage,
    parked: Option<Parked>,
    stream: Option<StreamId>,
    resizing: bool,
    latest_resize: Option<(PixelSize, f64)>,
    last_seq: u32,
}

impl E2 {
    pub(super) fn project(
        &mut self,
        window: WindowId,
        peer: NodeId,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let refusal = if !self.granted(peer, Capability::WindowShare) {
            Some(Refusal::Permission)
        } else if !self.permits_io() {
            Some(Refusal::Locked)
        } else if !self.windows.contains_key(&window)
            || !self.peers.contains(&peer)
            || self.sources.values().any(|s| s.window == window)
            || self.pending_parks.contains(&window)
            || self.next_projection.is_none()
            || !self.ledgers.recovery_done()
        {
            Some(Refusal::Busy)
        } else {
            None
        };
        if let Some(reason) = refusal {
            out.push(Output::Notice(Notice::ProjectionRefused { peer, reason }));
            return;
        }
        let (Some(info), Some(id)) = (self.windows.get(&window), self.next_projection) else {
            return;
        };
        self.next_projection = id.checked_add(1);
        let projection = ProjectionId(id);
        if self.ledgers.open(projection).is_err() {
            out.push(Output::Notice(Notice::ProjectionRefused {
                peer,
                reason: Refusal::InjectorFailed,
            }));
            return;
        }
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
        send(
            peer,
            Message::Start {
                projection,
                window: WindowSummary {
                    title: info.title.clone(),
                    app_id: info.app_id.clone(),
                },
                size,
            },
            out,
        );
        self.sources.insert(
            projection,
            Source {
                peer,
                window,
                stage: Stage::Offered(now.saturating_add(OFFER_TIMEOUT)),
                parked: None,
                stream: None,
                resizing: false,
                latest_resize: None,
                last_seq: 0,
            },
        );
    }

    pub(super) fn source_control(&mut self, peer: NodeId, msg: &Message, out: &mut Vec<Output>) {
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
            Message::Accepted { size, scale, .. } if matches!(source.stage, Stage::Offered(_)) => {
                source.stage = Stage::Parking;
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
                self.end_source(projection, Reason::Failed, true, out);
            }
            Message::Resize { size, scale, .. } if source.stage == Stage::Live => {
                if source.resizing {
                    source.latest_resize = Some((*size, *scale));
                } else {
                    source.resizing = true;
                    out.push(Output::ResizeParked {
                        window: source.window,
                        size: *size,
                        scale: *scale,
                    });
                }
            }
            Message::Focus { focused: true, .. } => {
                out.push(Output::ActivateWindow {
                    window: source.window,
                });
            }
            Message::KeyFrameRequest { .. } => {
                out.push(Output::RequestKeyFrame { projection });
            }
            Message::Close { reason, .. } => self.end_source(projection, *reason, true, out),
            _ => {}
        }
    }

    pub(super) fn parked(
        &mut self,
        window: WindowId,
        result: Result<Parked, Failure>,
        out: &mut Vec<Output>,
    ) {
        if self.pending_parks.remove(&window) {
            if result.is_ok() {
                out.push(Output::Restore { window });
            }
            return;
        }
        let Some((&projection, source)) = self
            .sources
            .iter_mut()
            .find(|(_, s)| s.window == window && (s.stage == Stage::Parking || s.resizing))
        else {
            return;
        };
        let initial = source.stage == Stage::Parking;
        // This operation has answered, so ending it must not wait for another Parked result.
        source.resizing = false;
        if initial {
            source.stage = Stage::Capturing;
        }
        let Ok(parked) = result else {
            self.end_source(projection, Reason::Failed, false, out);
            return;
        };
        let Some((size, parking)) = geometry(parked).filter(|_| parked.window == window) else {
            self.end_source(projection, Reason::Failed, false, out);
            return;
        };
        source.parked = Some(parked);
        if initial {
            let (target, crop) = match parking {
                ParkingKind::Twin => (CaptureTarget::Display(parked.display), Some(parked.content)),
                _ => (CaptureTarget::Window(window), None),
            };
            out.push(Output::StartCapture {
                projection,
                peer: source.peer,
                target,
                crop,
                max_fps: 60,
            });
            send(
                source.peer,
                Message::Geometry {
                    projection,
                    size,
                    parking,
                },
                out,
            );
            out.push(Output::Notice(Notice::ProjectionStarted {
                key: ProjectionKey {
                    source: self.node,
                    projection,
                },
                peer: source.peer,
                parking,
            }));
        } else {
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
            if let Some((size, scale)) = source.latest_resize.take() {
                source.resizing = true;
                out.push(Output::ResizeParked {
                    window,
                    size,
                    scale,
                });
            }
        }
    }

    pub(super) fn capture_started(
        &mut self,
        projection: ProjectionId,
        result: Result<StreamId, Failure>,
        out: &mut Vec<Output>,
    ) {
        if let Some(source) = self
            .sources
            .get_mut(&projection)
            .filter(|s| s.stage == Stage::Capturing)
        {
            match result {
                Ok(stream) => {
                    source.stream = Some(stream);
                    source.stage = Stage::Live;
                }
                Err(_) => self.end_source(projection, Reason::Failed, false, out),
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
            self.end_source(projection, reason, false, out);
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
                if self.focused != Some(source.window) {
                    out.push(Output::ActivateWindow {
                        window: source.window,
                    });
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
            self.end_source(projection, Reason::Failed, false, out);
        }
    }

    pub(super) fn end_source(
        &mut self,
        projection: ProjectionId,
        reason: Reason,
        peer_ended: bool,
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
        if source.stage == Stage::Parking || source.resizing {
            self.pending_parks.insert(source.window);
        }
        if !peer_ended {
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
            .filter(|(_, s)| matches!(s.stage, Stage::Offered(deadline) if deadline <= now))
            .map(|(&id, s)| (id, s.peer))
            .collect();
        for (id, peer) in expired {
            out.push(Output::Notice(Notice::ProjectionRefused {
                peer,
                reason: Refusal::Busy,
            }));
            self.end_source(id, Reason::Failed, false, out);
        }
    }

    pub(super) fn source_deadline(&self) -> Option<MonoTime> {
        self.sources
            .values()
            .filter_map(|s| match s.stage {
                Stage::Offered(deadline) => Some(deadline),
                _ => None,
            })
            .min()
    }

    pub(super) fn window_changed(&self, window: &WindowInfo, out: &mut Vec<Output>) {
        if self
            .windows
            .get(&window.id)
            .is_some_and(|old| old.title != window.title)
        {
            for (&projection, source) in &self.sources {
                if source.window == window.id {
                    send(
                        source.peer,
                        Message::Title {
                            projection,
                            title: window.title.clone(),
                        },
                        out,
                    );
                }
            }
        }
    }
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
