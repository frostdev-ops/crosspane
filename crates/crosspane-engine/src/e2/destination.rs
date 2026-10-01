//! Proxy lifecycle and deadline-driven input, resize and heartbeat coalescing.

use std::collections::BTreeSet;
use std::time::Duration;

use crosspane_input::Held;
use crosspane_input::timing::{HEARTBEAT_HELD, HEARTBEAT_IDLE};
use crosspane_protocol::msg::{Capability, InputMessage, MAX_HELD_KEYS, Refusal};
use crosspane_protocol::projection::{
    ProjInput, ProjectionEndReason as Reason, ProjectionMessage as Message,
};
use crosspane_types::geom::{PixelSize, PointDevice};
use crosspane_types::id::NodeId;
use crosspane_types::time::MonoTime;

use super::ledger::split;
use super::{E2, send};
use crate::io::{Failure, Notice, Output, ProjectionKey, ProxyEvent};

// Ceiling of 1 second / 120: rounding downward would exceed 120 Hz.
const MOTION_SLOT: Duration = Duration::from_nanos(8_333_334);
const RESIZE_SLOT: Duration = Duration::from_millis(50);
const KEYFRAME_SLOT: Duration = Duration::from_millis(200);
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const PEER_CAP: usize = 16;

pub(super) struct Destination {
    open: bool,
    open_due: Option<MonoTime>,
    seq: u32,
    held: BTreeSet<Held>,
    position: PointDevice,
    motion: Option<PointDevice>,
    motion_due: Option<MonoTime>,
    last_motion: Option<MonoTime>,
    resize: Option<(PixelSize, f64)>,
    resize_due: Option<MonoTime>,
    last_resize: Option<MonoTime>,
    last_heartbeat: MonoTime,
    heartbeat_due: Option<MonoTime>,
    last_keyframe: Option<MonoTime>,
}

impl Destination {
    fn input(
        &mut self,
        key: ProjectionKey,
        make: impl FnOnce(u32) -> ProjInput,
        out: &mut Vec<Output>,
    ) -> bool {
        let Some(seq) = self.seq.checked_add(1) else {
            return false;
        };
        self.seq = seq;
        out.push(Output::SendInput {
            peer: key.source,
            msg: InputMessage::Proj(make(seq)),
        });
        true
    }

    fn heartbeat_changed(&mut self, now: MonoTime) {
        let interval = if self.held.is_empty() {
            HEARTBEAT_IDLE
        } else {
            HEARTBEAT_HELD
        };
        self.heartbeat_due = Some(self.last_heartbeat.saturating_add(interval).max(now));
    }

    fn flush_motion(&mut self, key: ProjectionKey, now: MonoTime, out: &mut Vec<Output>) -> bool {
        // Buttons and scrolls force a flush even between timer slots: preserving discrete-event
        // order takes priority over the timer-driven motion rate cap.
        let Some(position) = self.motion.take() else {
            return true;
        };
        self.motion_due = None;
        self.last_motion = Some(now);
        self.input(
            key,
            |seq| ProjInput::Motion {
                projection: key.projection,
                seq,
                position,
            },
            out,
        )
    }

    fn ups(&mut self, key: ProjectionKey, out: &mut Vec<Output>) {
        for item in std::mem::take(&mut self.held) {
            let position = self.position;
            self.input(
                key,
                |seq| match item {
                    Held::Key(usage) => ProjInput::Key {
                        projection: key.projection,
                        seq,
                        usage,
                        down: false,
                    },
                    Held::Button(button) => ProjInput::Button {
                        projection: key.projection,
                        seq,
                        button,
                        down: false,
                        position,
                    },
                },
                out,
            );
        }
    }
}

impl E2 {
    pub(super) fn destination_control(
        &mut self,
        peer: NodeId,
        msg: &Message,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let projection = match msg {
            Message::Start { projection, .. }
            | Message::Geometry { projection, .. }
            | Message::Title { projection, .. }
            | Message::End { projection, .. } => *projection,
            _ => return,
        };
        let key = ProjectionKey {
            source: peer,
            projection,
        };
        if let Message::Start { window, size, .. } = msg {
            let reason = if !self.granted(peer, Capability::WindowPresent) {
                Some(Refusal::Permission)
            } else if !self.permits_io() {
                Some(Refusal::Locked)
            } else {
                None
            };
            if let Some(reason) = reason {
                send(peer, Message::Refused { projection, reason }, out);
                return;
            }
            if self.destinations.contains_key(&key) {
                return;
            }
            // Pending opens count too, so concurrent Start messages cannot bypass the cap.
            if self
                .destinations
                .keys()
                .filter(|key| key.source == peer)
                .count()
                >= PEER_CAP
            {
                send(
                    peer,
                    Message::Refused {
                        projection,
                        reason: Refusal::Busy,
                    },
                    out,
                );
                return;
            }
            self.destinations.insert(
                key,
                Destination {
                    open: false,
                    open_due: Some(now.saturating_add(OPEN_TIMEOUT)),
                    seq: 0,
                    held: BTreeSet::new(),
                    position: PointDevice::zero(),
                    motion: None,
                    motion_due: None,
                    last_motion: None,
                    resize: None,
                    resize_due: None,
                    last_resize: None,
                    last_heartbeat: now,
                    heartbeat_due: None,
                    last_keyframe: None,
                },
            );
            out.push(Output::OpenProxy {
                key,
                title: window.title.clone(),
                app_id: window.app_id.clone(),
                size: *size,
            });
            return;
        }
        let Some(destination) = self.destinations.get(&key) else {
            return;
        };
        match msg {
            Message::Geometry { size, parking, .. } if destination.open => {
                out.push(Output::ProxyGeometry {
                    key,
                    size: *size,
                    parking: *parking,
                })
            }
            Message::Title { title, .. } if destination.open => out.push(Output::ProxyTitle {
                key,
                title: title.clone(),
            }),
            Message::End { reason, .. } => self.end_destination(key, *reason, true, false, out),
            _ => {}
        }
    }

    pub(super) fn proxy_opened(
        &mut self,
        key: ProjectionKey,
        result: Result<(PixelSize, f64), Failure>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let Some(destination) = self.destinations.get_mut(&key) else {
            if result.is_ok() {
                out.push(Output::CloseProxy { key });
            }
            return;
        };
        if destination.open {
            return;
        }
        match result {
            Ok((size, scale)) => {
                destination.open = true;
                destination.open_due = None;
                destination.last_heartbeat = now;
                destination.heartbeat_due = now.checked_add(HEARTBEAT_IDLE);
                send(
                    key.source,
                    Message::Accepted {
                        projection: key.projection,
                        size,
                        scale,
                    },
                    out,
                );
            }
            Err(_) => {
                send(
                    key.source,
                    Message::Refused {
                        projection: key.projection,
                        reason: Refusal::InjectorFailed,
                    },
                    out,
                );
                out.push(Output::Notice(Notice::ProjectionRefused {
                    peer: key.source,
                    reason: Refusal::InjectorFailed,
                }));
                // Close is idempotent, including a partially-created or failed proxy.
                out.push(Output::CloseProxy { key });
                self.destinations.remove(&key);
            }
        }
    }

    pub(super) fn proxy_event(
        &mut self,
        key: ProjectionKey,
        event: &ProxyEvent,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        match event {
            ProxyEvent::CloseRequested => {
                self.end_destination(key, Reason::Returned, false, false, out);
                return;
            }
            ProxyEvent::Lost => {
                self.end_destination(key, Reason::Failed, false, false, out);
                return;
            }
            _ => {}
        }
        if !self.permits_io() {
            return;
        }
        let Some(destination) = self.destinations.get_mut(&key).filter(|d| d.open) else {
            return;
        };
        let mut sent = true;
        match event {
            ProxyEvent::Resized { size, scale } => {
                if destination
                    .last_resize
                    .is_none_or(|last| now.saturating_duration_since(last) >= RESIZE_SLOT)
                {
                    destination.resize = None;
                    destination.resize_due = None;
                    destination.last_resize = Some(now);
                    send(
                        key.source,
                        Message::Resize {
                            projection: key.projection,
                            size: *size,
                            scale: *scale,
                        },
                        out,
                    );
                } else {
                    destination.resize = Some((*size, *scale));
                    destination.resize_due = destination
                        .last_resize
                        .and_then(|last| last.checked_add(RESIZE_SLOT));
                }
            }
            ProxyEvent::Focus(focused) => {
                send(
                    key.source,
                    Message::Focus {
                        projection: key.projection,
                        focused: *focused,
                    },
                    out,
                );
                if !focused {
                    destination.ups(key, out);
                    destination.heartbeat_changed(now);
                }
            }
            ProxyEvent::CloseRequested => {
                self.end_destination(key, Reason::Returned, false, false, out);
                return;
            }
            ProxyEvent::Lost => {
                self.end_destination(key, Reason::Failed, false, false, out);
                return;
            }
            ProxyEvent::Key { usage, down } => {
                if *down
                    && !destination.held.contains(&Held::Key(*usage))
                    && destination
                        .held
                        .iter()
                        .filter(|item| matches!(item, Held::Key(_)))
                        .count()
                        >= MAX_HELD_KEYS
                {
                    sent = destination.input(
                        key,
                        |seq| ProjInput::Key {
                            projection: key.projection,
                            seq,
                            usage: *usage,
                            down: false,
                        },
                        out,
                    );
                    if !sent {
                        self.end_destination(key, Reason::Failed, false, false, out);
                    }
                    return;
                }
                if !transition(&mut destination.held, Held::Key(*usage), *down) {
                    return;
                }
                sent = destination.input(
                    key,
                    |seq| ProjInput::Key {
                        projection: key.projection,
                        seq,
                        usage: *usage,
                        down: *down,
                    },
                    out,
                );
                destination.heartbeat_changed(now);
            }
            ProxyEvent::Button {
                button,
                down,
                position,
            } => {
                if !(1..=16).contains(&button.0) {
                    // Unsupported buttons are never forwarded or held, so there is no up to send.
                    return;
                }
                if !transition(&mut destination.held, Held::Button(*button), *down) {
                    return;
                }
                destination.position = *position;
                sent = destination.flush_motion(key, now, out);
                sent &= destination.input(
                    key,
                    |seq| ProjInput::Button {
                        projection: key.projection,
                        seq,
                        button: *button,
                        down: *down,
                        position: *position,
                    },
                    out,
                );
                destination.heartbeat_changed(now);
            }
            ProxyEvent::Scroll { delta, position } => {
                destination.position = *position;
                sent = destination.flush_motion(key, now, out);
                sent &= destination.input(
                    key,
                    |seq| ProjInput::Scroll {
                        projection: key.projection,
                        seq,
                        delta: *delta,
                        position: *position,
                    },
                    out,
                );
            }
            ProxyEvent::Motion { position } => {
                destination.position = *position;
                destination.motion = Some(*position);
                if destination.last_motion.is_none() {
                    sent = destination.flush_motion(key, now, out);
                } else if destination.motion_due.is_none() {
                    destination.motion_due = destination.last_motion.map_or_else(
                        || now.checked_add(MOTION_SLOT),
                        |last| {
                            last.checked_add(MOTION_SLOT)
                                .map(|deadline| deadline.max(now))
                        },
                    );
                }
            }
        }
        if !sent {
            self.end_destination(key, Reason::Failed, false, false, out);
        }
    }

    pub(super) fn media_error(&mut self, key: ProjectionKey, now: MonoTime, out: &mut Vec<Output>) {
        if let Some(destination) = self.destinations.get_mut(&key).filter(|d| d.open)
            && destination
                .last_keyframe
                .is_none_or(|last| now.saturating_duration_since(last) >= KEYFRAME_SLOT)
        {
            destination.last_keyframe = Some(now);
            send(
                key.source,
                Message::KeyFrameRequest {
                    projection: key.projection,
                },
                out,
            );
        }
    }

    pub(super) fn end_destination(
        &mut self,
        key: ProjectionKey,
        reason: Reason,
        source_ended: bool,
        link_closed: bool,
        out: &mut Vec<Output>,
    ) {
        let Some(mut destination) = self.destinations.remove(&key) else {
            return;
        };
        if !link_closed {
            destination.ups(key, out);
        }
        if !source_ended && !link_closed {
            send(
                key.source,
                Message::Close {
                    projection: key.projection,
                    reason,
                },
                out,
            );
        }
        out.push(Output::CloseProxy { key });
        out.push(Output::Notice(Notice::ProjectionEnded { key, reason }));
    }

    pub(super) fn destination_tick(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let keys: Vec<_> = self.destinations.keys().copied().collect();
        for key in keys {
            if self
                .destinations
                .get(&key)
                .is_some_and(|d| d.open_due.is_some_and(|deadline| deadline <= now))
            {
                self.end_destination(key, Reason::Failed, false, false, out);
                continue;
            }
            let Some(destination) = self.destinations.get_mut(&key).filter(|d| d.open) else {
                continue;
            };
            let mut sent = true;
            if destination
                .motion_due
                .is_some_and(|deadline| deadline <= now)
            {
                sent = destination.flush_motion(key, now, out);
            }
            if destination
                .resize_due
                .is_some_and(|deadline| deadline <= now)
            {
                destination.resize_due = None;
                if let Some((size, scale)) = destination.resize.take() {
                    destination.last_resize = Some(now);
                    send(
                        key.source,
                        Message::Resize {
                            projection: key.projection,
                            size,
                            scale,
                        },
                        out,
                    );
                }
            }
            if destination
                .heartbeat_due
                .is_some_and(|deadline| deadline <= now)
            {
                let items: Vec<_> = destination.held.iter().copied().collect();
                let (keys, buttons) = split(&items);
                sent &= destination.input(
                    key,
                    |seq| ProjInput::Held {
                        projection: key.projection,
                        seq,
                        keys,
                        buttons,
                    },
                    out,
                );
                destination.last_heartbeat = now;
                let interval = if destination.held.is_empty() {
                    HEARTBEAT_IDLE
                } else {
                    HEARTBEAT_HELD
                };
                destination.heartbeat_due = now.checked_add(interval);
            }
            if !sent {
                self.end_destination(key, Reason::Failed, false, false, out);
            }
        }
    }

    pub(super) fn destination_deadline(&self) -> Option<MonoTime> {
        self.destinations
            .values()
            .flat_map(|d| [d.open_due, d.motion_due, d.resize_due, d.heartbeat_due])
            .flatten()
            .min()
    }
}

fn transition(held: &mut BTreeSet<Held>, item: Held, down: bool) -> bool {
    if down {
        held.insert(item)
    } else {
        held.remove(&item)
    }
}
