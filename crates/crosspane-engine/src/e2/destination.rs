//! Proxy lifecycle and deadline-driven input, resize and heartbeat coalescing.

use std::collections::BTreeSet;
use std::time::Duration;

use crosspane_input::Held;
use crosspane_input::timing::{HEARTBEAT_HELD, HEARTBEAT_IDLE};
use crosspane_protocol::msg::{Capability, InputMessage, MAX_HELD_KEYS, Refusal};
use crosspane_protocol::projection::{
    ParkingKind, ProjInput, ProjectionEndReason as Reason, ProjectionMessage as Message,
};
use crosspane_types::geom::{PixelSize, PointDevice};
use crosspane_types::id::{DisplayId, NodeId};
use crosspane_types::time::MonoTime;

use super::ledger::split;
use super::{E2, GRACE, send};
use crate::io::{Command, Failure, Notice, Output, ProjectionKey, ProxyEvent};

// Ceiling of 1 second / 120: rounding downward would exceed 120 Hz.
const MOTION_SLOT: Duration = Duration::from_nanos(8_333_334);
const RESIZE_SLOT: Duration = Duration::from_millis(50);
/// After the last user resize, the user counts as still resizing for this long: the source's
/// answer is held back that long, so it never fights a drag in progress.
const RESIZE_QUIET: Duration = Duration::from_millis(250);
/// How long after a size was asked of the host a matching callback still counts as the host's
/// own resize. It bounds classification only (a slow callback is read as the user's, costing one
/// request); the unconfirmed command itself is remembered until confirmed or superseded.
const COMMAND_TTL: Duration = Duration::from_secs(1);
const KEYFRAME_SLOT: Duration = Duration::from_millis(200);
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const PEER_CAP: usize = 16;

/// A host reported the requested proxy size even after the source answered with another size
/// and we commanded that answer. One observation still permits WP-2.34's genuine renewed
/// request; the same exchange twice records that the host keeps its size instead.
#[derive(Clone, Copy)]
struct HostRefusal {
    asked: PixelSize,
    scale: f64,
    answered: PixelSize,
    settled: bool,
}

pub(super) struct Destination {
    pub(super) title: String,
    open: bool,
    pub(super) clipboard_focused: bool,
    open_due: Option<MonoTime>,
    suspended: Option<MonoTime>,
    /// Latest proxy geometry, including resizes not yet sent over the link.
    current: Option<(PixelSize, f64)>,
    seq: u32,
    held: BTreeSet<Held>,
    position: PointDevice,
    motion: Option<PointDevice>,
    motion_due: Option<MonoTime>,
    last_motion: Option<MonoTime>,
    resize: Option<(PixelSize, f64)>,
    resize_due: Option<MonoTime>,
    last_resize: Option<MonoTime>,
    /// The size and scale last sent to the source (Accepted or Resize): a proxy that reports
    /// the same size again is not a resize (window systems repeat configures). Only this side's
    /// own requests write it, never the source's `Geometry`.
    last_sent: Option<(PixelSize, f64)>,
    /// The number of the last `Resize` sent: 0 before the first. A `Geometry` resizes the proxy
    /// only if it answers this one.
    request: u32,
    /// The window's actual content size the source reported in answer to `last_sent`'s request:
    /// `None` while that request is outstanding. (The source parks at the scale it was asked
    /// for, so the scale can't differ.) A size the user drags back to is a new request when the
    /// source is known to be at a different one.
    acknowledged: Option<PixelSize>,
    /// The newest size asked of the host (`ProxyGeometry`) that the host hasn't confirmed yet,
    /// with the scale and the time then. A newer command replaces it, so it always equals the
    /// newest source geometry applied; a matching callback (within `COMMAND_TTL`) confirms it;
    /// user intent clears it. Its age never makes it irrelevant: it says what the proxy is about
    /// to be, however late the host reports it. The host reports its own resizes through the
    /// same event as the user's, so matching is a heuristic only: correctness rests on request
    /// numbers, and either misreading is restored by one more request.
    commanded: Option<(PixelSize, f64, MonoTime)>,
    /// An explicit, known difference between host and source, not an outstanding resize.
    refusal: Option<HostRefusal>,
    /// The parking kind of the last `ProxyGeometry` emitted (none before the first).
    parking: Option<ParkingKind>,
    /// The user counts as resizing until this time (extended by every user `Resized`).
    active_until: Option<MonoTime>,
    /// The source's answer to the newest request, held while the user is resizing: applied when
    /// the user is quiet. Only ever set while `active_until` is.
    held_geometry: Option<(PixelSize, ParkingKind)>,
    last_heartbeat: MonoTime,
    heartbeat_due: Option<MonoTime>,
    last_keyframe: Option<MonoTime>,
    /// WP-2.43 §4 "placement production": the generation of the newest `ProxyPlaced` (0 before
    /// the first). It grows by one with every change and never restarts while the projection
    /// lives; `u32::MAX` is reserved for the terminal invalidation (A5).
    placed_gen: u32,
    /// What the newest report said (the display is `None` when the proxy is on none), kept so a
    /// repeat is dropped and a reconnect can resend it with `placed_gen` unchanged.
    placed_last: Option<(Option<DisplayId>, PointDevice, PixelSize)>,
    /// The terminal report was sent: nothing more is reported for this projection.
    placed_final: bool,
    drag: Option<(u32, (i32, i32), bool)>,
}

impl Destination {
    pub(super) fn drag_geometry(&self) -> Option<(PixelSize, f64)> {
        self.current
            .filter(|_| self.open && self.suspended.is_none())
    }
    /// A `ProxyEvent::Placed` (WP-2.43 §4). A report equal to the last is dropped; otherwise the
    /// generation grows by one, and a destination whose next generation would be `u32::MAX` sends
    /// one final report with `display: None` and then nothing more. While suspended the newest
    /// report is only recorded: it is resent after `Accepted`.
    fn placed(
        &mut self,
        key: ProjectionKey,
        display: Option<DisplayId>,
        origin: PointDevice,
        size: PixelSize,
        out: &mut Vec<Output>,
    ) {
        if self.placed_final {
            return;
        }
        // A host that reports a non-finite origin has not told us where the proxy is.
        let (display, origin) = if origin.x.is_finite() && origin.y.is_finite() {
            (display, origin)
        } else {
            (None, PointDevice::zero())
        };
        if self.placed_last == Some((display, origin, size)) {
            return;
        }
        match self
            .placed_gen
            .checked_add(1)
            .filter(|next| *next < u32::MAX)
        {
            Some(next) => {
                self.placed_gen = next;
                self.placed_last = Some((display, origin, size));
            }
            None => {
                self.placed_gen = u32::MAX;
                self.placed_final = true;
                self.placed_last = Some((None, origin, size));
            }
        }
        if self.suspended.is_none() {
            self.send_placed(key, out);
        }
    }

    fn send_placed(&self, key: ProjectionKey, out: &mut Vec<Output>) {
        if let Some((display, origin, size)) = self.placed_last {
            send(
                key.source,
                Message::ProxyPlaced {
                    projection: key.projection,
                    generation: self.placed_gen,
                    display,
                    origin,
                    size,
                },
                out,
            );
        }
    }

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

    /// Whether the user is resizing the proxy right now (or just was).
    fn user_active(&self, now: MonoTime) -> bool {
        self.resize.is_some() || self.active_until.is_some_and(|until| now < until)
    }

    /// Whether a user resize to `size` at `scale` repeats what the source already has: it is the
    /// last request, and that is still outstanding or was met exactly. If the source answered
    /// with another size (an app's minimum), the user's renewed request is a genuine one.
    fn repeats(&self, size: PixelSize, scale: f64) -> bool {
        self.last_sent == Some((size, scale))
            && self.acknowledged.is_none_or(|actual| actual == size)
    }

    /// Send a `Resize` carrying the next request number. Returns false when the numbers run out.
    fn send_resize(
        &mut self,
        key: ProjectionKey,
        size: PixelSize,
        scale: f64,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> bool {
        let Some(request) = self.request.checked_add(1) else {
            return false;
        };
        self.request = request;
        self.last_resize = Some(now);
        self.last_sent = Some((size, scale));
        self.acknowledged = None;
        // The held answer was to an older request: a newer answer will come.
        self.held_geometry = None;
        send(
            key.source,
            Message::Resize {
                fullscreen: false,
                projection: key.projection,
                request,
                size,
                scale,
            },
            out,
        );
        true
    }

    /// The source's answer to the newest request: resize the proxy to it if it differs, and tell
    /// the host about a changed parking kind even at an unchanged size.
    fn apply_geometry(
        &mut self,
        key: ProjectionKey,
        size: PixelSize,
        parking: ParkingKind,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let Some((current, scale)) = self.current else {
            return;
        };
        // What the proxy is, or is about to be: a size already asked for and not yet confirmed
        // counts (whatever its age), or an answer that returns to the old size would never be
        // sent.
        let expected = self.refusal.filter(|r| r.settled).map_or_else(
            || self.commanded.map_or(current, |(asked, _, _)| asked),
            |r| r.answered,
        );
        let resizes = expected != size;
        if !resizes && self.parking == Some(parking) {
            return;
        }
        if resizes {
            // The proxy is about to have this size, and its callback is the host's own. If it
            // already has it (the answer undoes an unconfirmed command), there is nothing to
            // wait for.
            self.commanded = (size != current).then_some((size, scale, now));
        }
        self.parking = Some(parking);
        out.push(Output::ProxyGeometry { key, size, parking });
    }

    /// Whether a `Resized` changes nothing: it is the size and scale the proxy already has.
    /// While a host resize is unconfirmed (commanded, its callback not here yet), `current` is
    /// not what the window will be, so a report of `current` could just as well be the user
    /// dragging back onto it, however late the callback is; ignoring that would leave the proxy
    /// and the source apart with nothing in flight. It is then not "unchanged", and the user's.
    fn unchanged(&self, size: PixelSize, scale: f64) -> bool {
        self.current == Some((size, scale))
            && self
                .commanded
                .is_none_or(|(asked, at, _)| (asked, at) == (size, scale))
    }

    /// A `Resized` from the host (other than one that changes nothing). If it matches the
    /// unconfirmed host resize this side asked for, and comes within `COMMAND_TTL` of it, it is
    /// that completing: it confirms the record and the caller sends nothing. Anything else is
    /// the user's, including a late callback for an older command (the proxy then sits at a size
    /// the source is no longer at) and a callback that took longer than the TTL.
    fn programmatic(&mut self, size: PixelSize, scale: f64, now: MonoTime) -> bool {
        let Some((asked, at, issued)) = self.commanded else {
            return false;
        };
        if asked != size || at != scale || !live(issued, now) {
            return false;
        }
        self.current = Some((size, scale));
        self.commanded = None;
        self.refusal = None;
        true
    }

    /// A callback contradicting a command is ambiguous once (M5). Repeating the exact refused
    /// exchange settles it: the newest request is answered, the host stays where it reported,
    /// and later identical reports/answers need no more traffic. No TTL can manufacture intent.
    fn host_refused(&mut self, size: PixelSize, scale: f64) -> bool {
        let Some((answered, at, _)) = self.commanded else {
            return false;
        };
        if at != scale
            || answered == size
            || self.last_sent != Some((size, scale))
            || self.acknowledged != Some(answered)
        {
            return false;
        }
        let settled = self
            .refusal
            .is_some_and(|r| r.asked == size && r.scale == scale && r.answered == answered);
        self.refusal = Some(HostRefusal {
            asked: size,
            scale,
            answered,
            settled,
        });
        if settled {
            self.current = Some((size, scale));
            self.commanded = None;
            self.resize = None;
            self.resize_due = None;
            self.held_geometry = None;
            self.active_until = None;
        }
        settled
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
    pub(super) fn browse_command(&self, command: Command, out: &mut Vec<Output>) {
        let (peer, request, msg) = match command {
            Command::Browse { peer, request } => (peer, request, Message::ListWindows { request }),
            Command::Pull {
                peer,
                window,
                request,
            } => (peer, request, Message::Pull { request, window }),
            _ => return,
        };
        if self.peers.contains(&peer) {
            send(peer, msg, out);
        } else {
            out.push(Output::BrowseResult {
                peer,
                request,
                result: Err(Refusal::Busy),
            });
        }
    }

    pub(super) fn destination_control(
        &mut self,
        peer: NodeId,
        msg: &Message,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        match msg {
            Message::WindowList { request, windows } => {
                out.push(Output::BrowseResult {
                    peer,
                    request: *request,
                    result: Ok(windows.clone()),
                });
                return;
            }
            Message::BrowseRefused { request, reason } => {
                out.push(Output::BrowseResult {
                    peer,
                    request: *request,
                    result: Err(*reason),
                });
                return;
            }
            _ => {}
        }
        let projection = match msg {
            Message::Start { projection, .. }
            | Message::StartAt { projection, .. }
            | Message::Geometry { projection, .. }
            | Message::Title { projection, .. }
            | Message::DragCancel { projection, .. }
            | Message::End { projection, .. } => *projection,
            _ => return,
        };
        let key = ProjectionKey {
            source: peer,
            projection,
        };
        if let Message::Start { window, size, .. } | Message::StartAt { window, size, .. } = msg {
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
            if let Some(destination) = self.destinations.get_mut(&key) {
                if let Some(deadline) = destination.suspended {
                    if deadline <= now {
                        self.end_destination(key, Reason::LinkLost, true, true, out);
                        return;
                    }
                    destination.suspended = None;
                    destination.refusal = None;
                    if let Some((size, scale)) = destination.current.filter(|_| destination.open) {
                        destination.last_sent = Some((size, scale));
                        destination.acknowledged = None;
                        destination.last_heartbeat = now;
                        destination.heartbeat_due = now.checked_add(HEARTBEAT_IDLE);
                        send(
                            peer,
                            Message::Accepted {
                                projection,
                                size,
                                scale,
                            },
                            out,
                        );
                        // A restarted stream's first frame is a key frame (E2-v0, decision 3).
                        // MediaError requests another if that frame cannot be applied.
                        // Correlation starts afresh: a request the disconnect discarded can
                        // never be answered, so ask again, for the size the proxy has now.
                        if !destination.send_resize(key, size, scale, now, out) {
                            self.end_destination(key, Reason::Failed, false, false, out);
                            return;
                        }
                        // The source dropped the placement with the connection (it keeps only
                        // its high-water mark): repeat the newest report, generation unchanged.
                        destination.send_placed(key, out);
                    } else {
                        // The old OpenProxy is still in flight: wait for it, never open twice.
                        destination.open_due = Some(now.saturating_add(OPEN_TIMEOUT));
                    }
                }
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
                    title: window.title.clone(),
                    open: false,
                    clipboard_focused: false,
                    open_due: Some(now.saturating_add(OPEN_TIMEOUT)),
                    suspended: None,
                    current: None,
                    seq: 0,
                    held: BTreeSet::new(),
                    position: PointDevice::zero(),
                    motion: None,
                    motion_due: None,
                    last_motion: None,
                    resize: None,
                    resize_due: None,
                    last_resize: None,
                    last_sent: None,
                    request: 0,
                    acknowledged: None,
                    commanded: None,
                    refusal: None,
                    parking: None,
                    active_until: None,
                    held_geometry: None,
                    last_heartbeat: now,
                    heartbeat_due: None,
                    last_keyframe: None,
                    placed_gen: 0,
                    placed_last: None,
                    placed_final: false,
                    drag: match msg {
                        Message::StartAt {
                            place,
                            token,
                            anchor,
                            ..
                        } if place.drag => Some((*token, *anchor, false)),
                        _ => None,
                    },
                },
            );
            out.push(Output::OpenProxy {
                key,
                title: window.title.clone(),
                app_id: window.app_id.clone(),
                size: *size,
                place: match msg {
                    Message::StartAt { place, .. } => Some(*place),
                    _ => None,
                },
            });
            return;
        }
        let Some(destination) = self.destinations.get_mut(&key) else {
            return;
        };
        match msg {
            Message::DragCancel { token, .. } => {
                if destination
                    .drag
                    .is_some_and(|(known, _, _)| known == *token)
                {
                    destination.drag = None;
                }
            }
            Message::Geometry {
                size,
                parking,
                answers,
                ..
            } if destination.open && destination.suspended.is_none() => {
                // Only the answer to the newest request can resize the proxy. An older one
                // (including any from a source that predates request numbers) shows the window
                // as it was before that request: a newer answer will come, and no timer
                // promotes this one.
                if *answers == destination.request {
                    if destination.refusal.is_some_and(|r| r.answered != *size) {
                        // A different current source geometry begins a fresh exchange, including
                        // a source-initiated resize carrying the last answered request number.
                        destination.refusal = None;
                    }
                    destination.acknowledged = Some(*size);
                    if destination.user_active(now) {
                        // Keep only the current answer, until the user is quiet.
                        destination.held_geometry = Some((*size, *parking));
                    } else {
                        // This answer is the newest, even if it needs no command: it
                        // supersedes any older one still held for the quiet deadline.
                        destination.held_geometry = None;
                        destination.apply_geometry(key, *size, *parking, now, out);
                    }
                }
            }
            Message::Title { title, .. } if destination.open && destination.suspended.is_none() => {
                destination.title = title.clone();
                out.push(Output::ProxyTitle {
                    key,
                    title: title.clone(),
                })
            }
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
                destination.current = Some((size, scale));
                if destination.suspended.is_some() {
                    return;
                }
                destination.last_sent = Some((size, scale));
                destination.acknowledged = None;
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
                if destination.suspended.is_none() {
                    send(
                        key.source,
                        Message::Refused {
                            projection: key.projection,
                            reason: Refusal::InjectorFailed,
                        },
                        out,
                    );
                }
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
        if let ProxyEvent::Placed {
            display,
            origin,
            size,
        } = event
        {
            // Not a user action: recorded even while suspended, sent when the link is up.
            destination.placed(key, *display, *origin, *size, out);
            if destination.suspended.is_none()
                && let Some((token, anchor, false)) = destination.drag
                && let Some(display) = display.filter(|id| self.display_bounds.contains_key(id))
                && anchor.0 >= 0
                && anchor.1 >= 0
                && (anchor.0 as u32) < size.width
                && (anchor.1 as u32) < size.height
                && origin.x.is_finite()
                && origin.y.is_finite()
            {
                destination.drag = Some((token, anchor, true));
                send(
                    key.source,
                    Message::DragReady {
                        projection: key.projection,
                        token,
                        display,
                        position: *origin
                            + crosspane_types::geom::VectorDevice::new(
                                f64::from(anchor.0),
                                f64::from(anchor.1),
                            ),
                    },
                    out,
                );
            }
            return;
        }
        if let ProxyEvent::Resized { size, scale } = event {
            if destination
                .refusal
                .is_some_and(|r| r.asked != *size || r.scale != *scale)
            {
                // Observable new user intent (floating/dragging or changing display scale).
                destination.refusal = None;
            }
            if destination.unchanged(*size, *scale) {
                // Nothing changed (the host can report one change twice).
                return;
            }
            if destination.suspended.is_none() && destination.programmatic(*size, *scale, now) {
                // The completion of a size this side asked the host for: not the user's.
                return;
            }
            if destination.suspended.is_none() && destination.host_refused(*size, *scale) {
                return;
            }
            // The user's: it supersedes the outstanding host resize, whose late callback is then
            // read as the user's too (one harmless extra request).
            destination.commanded = None;
            destination.current = Some((*size, *scale));
        }
        if destination.suspended.is_some() {
            return;
        }
        let mut sent = true;
        match event {
            ProxyEvent::Resized { size, scale } if destination.repeats(*size, *scale) => {
                // The user dragged back to the size the source already has; drop any coalesced
                // resize that would undo it.
                destination.active_until = Some(now.saturating_add(RESIZE_QUIET));
                destination.resize = None;
                destination.resize_due = None;
            }
            ProxyEvent::Resized { size, scale } => {
                destination.active_until = Some(now.saturating_add(RESIZE_QUIET));
                if destination
                    .last_resize
                    .is_none_or(|last| now.saturating_duration_since(last) >= RESIZE_SLOT)
                {
                    destination.resize = None;
                    destination.resize_due = None;
                    sent = destination.send_resize(key, *size, *scale, now, out);
                } else {
                    destination.resize = Some((*size, *scale));
                    destination.resize_due = destination
                        .last_resize
                        .and_then(|last| last.checked_add(RESIZE_SLOT));
                }
            }
            ProxyEvent::Focus(focused) => {
                destination.clipboard_focused = *focused;
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
            // Forwarded as `ProxyPlaced` before this match (WP-2.43b).
            ProxyEvent::Placed { .. } => {}
            ProxyEvent::Fullscreen(_) => {}
        }
        if !sent {
            self.end_destination(key, Reason::Failed, false, false, out);
        }
    }

    pub(super) fn media_error(&mut self, key: ProjectionKey, now: MonoTime, out: &mut Vec<Output>) {
        if let Some(destination) = self
            .destinations
            .get_mut(&key)
            .filter(|d| d.open && d.suspended.is_none())
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

    pub(super) fn suspend_destination(&mut self, key: ProjectionKey, now: MonoTime) {
        if let Some(destination) = self.destinations.get_mut(&key) {
            destination.held.clear();
            destination.drag = None;
            destination.suspended = Some(now.saturating_add(GRACE));
            destination.open_due = None;
            destination.motion = None;
            destination.motion_due = None;
            destination.last_motion = None;
            destination.resize = None;
            destination.resize_due = None;
            destination.last_resize = None;
            destination.acknowledged = None;
            destination.commanded = None;
            destination.refusal = None;
            destination.active_until = None;
            destination.held_geometry = None;
            destination.heartbeat_due = None;
            destination.last_keyframe = None;
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
        let link_closed = link_closed || destination.suspended.is_some();
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
            if let Some(deadline) = self.destinations.get(&key).and_then(|d| d.suspended) {
                if deadline <= now {
                    self.end_destination(key, Reason::LinkLost, true, true, out);
                }
                continue;
            }
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
                if let Some((size, scale)) = destination.resize.take()
                    && !destination.repeats(size, scale)
                {
                    sent &= destination.send_resize(key, size, scale, now, out);
                }
            }
            // The user has stopped: the source's answer to the newest request may resize the
            // proxy now.
            if destination.resize.is_none()
                && destination.active_until.is_none_or(|until| until <= now)
                && let Some((size, parking)) = destination.held_geometry.take()
            {
                destination.apply_geometry(key, size, parking, now, out);
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
            .flat_map(|d| {
                [
                    d.open_due,
                    d.suspended,
                    d.motion_due,
                    d.resize_due,
                    d.heartbeat_due,
                    // A held answer waits for the user to be quiet (until then, a pending
                    // resize's own deadline comes first and drops it).
                    d.held_geometry
                        .filter(|_| d.resize.is_none())
                        .and(d.active_until),
                ]
            })
            .flatten()
            .min()
    }
}

/// Whether a host resize issued at `issued` is still recognisable at `now`.
fn live(issued: MonoTime, now: MonoTime) -> bool {
    now.saturating_duration_since(issued) < COMMAND_TTL
}

fn transition(held: &mut BTreeSet<Held>, item: Held, down: bool) -> bool {
    if down {
        held.insert(item)
    } else {
        held.remove(&item)
    }
}

#[cfg(test)]
mod tests {
    //! The placement generation's boundary (WP-2.43 A5): a destination can't be driven through
    //! 2^32 changes from outside the crate, so the counter is set next to its limit directly.

    use crosspane_input::journal::MemoryJournal;
    use crosspane_platform::{LockState, SessionEvent, SessionState};
    use crosspane_protocol::link::LinkEvent;
    use crosspane_protocol::msg::ControlMessage;
    use crosspane_types::id::ProjectionId;

    use super::*;
    use crate::config::EngineConfig;
    use crate::io::Input;

    const SOURCE: NodeId = NodeId([1; 32]);
    const THIS: NodeId = NodeId([2; 32]);
    const DISPLAY: DisplayId = DisplayId(1);

    fn key() -> ProjectionKey {
        ProjectionKey {
            source: SOURCE,
            projection: ProjectionId(1),
        }
    }

    fn start() -> Input {
        Input::Link(LinkEvent::Control {
            peer: SOURCE,
            msg: ControlMessage::Projection(Message::Start {
                projection: ProjectionId(1),
                window: crosspane_protocol::projection::WindowSummary {
                    title: "t".into(),
                    app_id: "a".into(),
                },
                size: PixelSize::new(400, 300),
            }),
        })
    }

    fn destination() -> (E2, Vec<Output>) {
        let (mut e2, _) = E2::new(
            &EngineConfig::new(THIS),
            Box::new(MemoryJournal::default()),
            MonoTime::ZERO,
        )
        .unwrap();
        let mut out = Vec::new();
        e2.handle(
            &Input::Session(SessionEvent::State(SessionState {
                lock: LockState::Unlocked,
                active: Some(true),
            })),
            MonoTime::ZERO,
            &mut out,
        );
        e2.handle(&Input::PeerUp { peer: SOURCE }, MonoTime::ZERO, &mut out);
        e2.handle(
            &Input::Grants([(SOURCE, [Capability::WindowPresent].into())].into()),
            MonoTime::ZERO,
            &mut out,
        );
        e2.handle(&start(), MonoTime::ZERO, &mut out);
        e2.handle(
            &Input::ProxyOpened {
                key: key(),
                result: Ok((PixelSize::new(400, 300), 1.0)),
            },
            MonoTime::ZERO,
            &mut out,
        );
        (e2, out)
    }

    fn placed(e2: &mut E2, x: f64) -> Vec<(u32, Option<DisplayId>)> {
        let mut out = Vec::new();
        e2.handle(
            &Input::Proxy {
                key: key(),
                event: ProxyEvent::Placed {
                    display: Some(DISPLAY),
                    origin: PointDevice::new(x, 0.0),
                    size: PixelSize::new(400, 300),
                },
            },
            MonoTime::ZERO,
            &mut out,
        );
        reports(&out)
    }

    fn reports(out: &[Output]) -> Vec<(u32, Option<DisplayId>)> {
        out.iter()
            .filter_map(|o| match o {
                Output::SendControl {
                    msg:
                        ControlMessage::Projection(Message::ProxyPlaced {
                            generation,
                            display,
                            ..
                        }),
                    ..
                } => Some((*generation, *display)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn settled_refusal_records_the_difference_without_an_outstanding_request() {
        let (mut e2, _) = destination();
        let tile = PixelSize::new(300, 200);
        let minimum = PixelSize::new(400, 300);
        let d = e2.destinations.get_mut(&key()).unwrap();
        d.current = Some((tile, 1.0));
        d.last_sent = Some((tile, 1.0));
        d.acknowledged = Some(minimum);
        d.parking = Some(ParkingKind::Twin);
        d.commanded = Some((minimum, 1.0, MonoTime::ZERO));
        assert!(
            !d.host_refused(tile, 1.0),
            "first refusal retains M5's renewed request"
        );
        assert!(!d.refusal.unwrap().settled);
        d.request = 2;
        assert!(d.host_refused(tile, 1.0));
        let refusal = d.refusal.unwrap();
        assert!(refusal.settled);
        assert_eq!(
            (refusal.asked, refusal.scale, refusal.answered),
            (tile, 1.0, minimum)
        );
        assert_eq!(d.current, Some((tile, 1.0)));
        assert_eq!(d.acknowledged, Some(minimum));
        assert!(d.commanded.is_none() && d.resize.is_none() && d.held_geometry.is_none());
        let mut out = Vec::new();
        d.apply_geometry(key(), minimum, ParkingKind::Twin, MonoTime::ZERO, &mut out);
        assert!(out.is_empty(), "the known difference is not a new command");
        e2.proxy_event(
            key(),
            &ProxyEvent::Resized {
                size: PixelSize::new(500, 400),
                scale: 1.0,
            },
            MonoTime::ZERO,
            &mut out,
        );
        let d = e2.destinations.get(&key()).unwrap();
        assert!(d.refusal.is_none());
        assert_eq!(d.request, 3);
        assert!(
            d.acknowledged.is_none(),
            "a genuine new request is outstanding"
        );
    }

    #[test]
    fn generation_overflow_sends_one_terminal_report_and_stops() {
        let (mut e2, _) = destination();
        e2.destinations
            .get_mut(&key())
            .expect("destination")
            .placed_gen = u32::MAX - 3;
        // Ordinary reports up to u32::MAX - 1.
        assert_eq!(placed(&mut e2, 1.0), vec![(u32::MAX - 2, Some(DISPLAY))]);
        assert_eq!(placed(&mut e2, 2.0), vec![(u32::MAX - 1, Some(DISPLAY))]);
        // The next would be u32::MAX, which is reserved: one final report without a display.
        assert_eq!(placed(&mut e2, 3.0), vec![(u32::MAX, None)]);
        // Then nothing more for this projection, whatever the host says.
        assert_eq!(placed(&mut e2, 4.0), vec![]);
        assert_eq!(placed(&mut e2, 5.0), vec![]);
        // The terminal report is what a reconnect repeats.
        let mut out = Vec::new();
        e2.handle(
            &Input::Link(LinkEvent::Closed {
                peer: SOURCE,
                error: crosspane_protocol::link::LinkError::Closed,
            }),
            MonoTime::ZERO,
            &mut out,
        );
        e2.handle(&Input::PeerUp { peer: SOURCE }, MonoTime::ZERO, &mut out);
        out.clear();
        e2.handle(&start(), MonoTime::ZERO, &mut out);
        assert_eq!(reports(&out), vec![(u32::MAX, None)]);
    }

    #[test]
    fn generation_overflow_by_checked_add_is_also_terminal() {
        // A counter already at the limit (cannot happen through reports, but a corrupt one must
        // not wrap): the next change is the terminal report.
        let (mut e2, _) = destination();
        e2.destinations
            .get_mut(&key())
            .expect("destination")
            .placed_gen = u32::MAX;
        assert_eq!(placed(&mut e2, 1.0), vec![(u32::MAX, None)]);
        assert_eq!(placed(&mut e2, 2.0), vec![]);
    }
}
