//! E2 window projection roles (docs/wp/E2-v0.md): the source side (the window's owner) and the
//! destination side (the proxy), side by side. All time is supplied by the caller.

mod destination;
mod ledger;
mod source;

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crosspane_input::journal::{Journal, JournalError};
use crosspane_platform::{LockState, SessionEvent, SessionState, WindowEvent, WindowInfo};
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{Capability, ControlMessage, InputMessage};
use crosspane_protocol::projection::{ProjectionEndReason as Reason, ProjectionMessage as Message};
use crosspane_types::geom::{DisplayGeometry, PixelRect, PixelSize, PointDevice};
use crosspane_types::id::{DisplayId, NodeId, ProjectionId, WindowId};
use crosspane_types::time::MonoTime;

use crate::config::EngineConfig;
use crate::io::{Command, Input, Output, ProjectionKey};
use destination::Destination;
use ledger::Ledgers;
use source::Source;

const GRACE: Duration = Duration::from_secs(20);

/// N's newest accepted report of where P's proxy content is (WP-2.43 §2.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placement {
    pub generation: u32,
    pub display: DisplayId,
    pub origin: PointDevice,
    pub size: PixelSize,
}

/// A twin-parked window of this node that `peer` shows, for the E1 controller. Only live
/// sources with `ParkingKind::Twin` are listed; `placed` is `Some` only when the newest accepted
/// report is valid for this connection (WP-2.43 §4 "placement state"), has `display: Some` and
/// `size == content.size()`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TwinHome {
    pub peer: NodeId,
    pub projection: ProjectionId,
    pub window: WindowId,
    pub display: DisplayId,
    pub content: PixelRect,
    pub placed: Option<Placement>,
    /// `focus_on(windows, focused, window, display)` right now.
    pub focused: bool,
}

/// Both E2 roles of one node.
pub struct E2 {
    node: NodeId,
    state: SessionState,
    asleep: bool,
    awaiting_state: bool,
    panic: bool,
    peers: BTreeSet<NodeId>,
    grants: BTreeMap<NodeId, BTreeSet<Capability>>,
    windows: BTreeMap<WindowId, WindowInfo>,
    scales: BTreeMap<DisplayId, f64>,
    pub(super) display_bounds: BTreeMap<DisplayId, DisplayGeometry>,
    proxy_windows: BTreeMap<WindowId, ProjectionKey>,
    focused: Option<WindowId>,
    /// The window that had focus before a parked window was activated for its proxy: focus
    /// goes back to it when the proxy loses focus, so this node's own new windows don't open
    /// on the invisible twin display.
    focus_before: Option<WindowId>,
    next_projection: Option<u64>,
    sources: BTreeMap<ProjectionId, Source>,
    destinations: BTreeMap<ProjectionKey, Destination>,
    // Don't reuse a window while its previous parking operation can still complete.
    pending_parks: BTreeMap<WindowId, MonoTime>,
    pending_places: BTreeMap<WindowId, crosspane_protocol::projection::ProxyPlacement>,
    ledgers: Ledgers,
    /// WP-2.43 §2.4: while the E1 controller is home (entering, home, exiting, or still removing
    /// its bind), this node's physical input owns the seat: no source injects anything but
    /// releases, and no focus request is acted on.
    home: Option<(NodeId, ProjectionId)>,
}

impl fmt::Debug for E2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("E2")
            .field("source_count", &self.sources.len())
            .field("destination_count", &self.destinations.len())
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl E2 {
    /// Accepted focus only; pending wishes and ignored raw events never appear here.
    pub(crate) fn clipboard_source_focus(
        &self,
    ) -> impl Iterator<Item = (ProjectionId, NodeId)> + '_ {
        self.sources
            .iter()
            .filter_map(|(id, source)| source.clipboard_focused.then_some((*id, source.peer)))
    }

    pub(crate) fn clipboard_destination_focus(&self) -> impl Iterator<Item = ProjectionKey> + '_ {
        self.destinations
            .iter()
            .filter_map(|(key, destination)| destination.clipboard_focused.then_some(*key))
    }

    pub(crate) fn cleanup_item(
        &mut self,
        item: crosspane_input::Held,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        self.ledgers.cleanup_press(item, now, out);
    }

    pub(crate) fn physical(&self) -> crate::physical_input::PhysicalInput {
        self.ledgers.physical()
    }

    pub(crate) fn set_physical(
        &mut self,
        physical: crate::physical_input::PhysicalInput,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> Result<(), JournalError> {
        self.ledgers.set_physical(physical, now, out)
    }

    /// Start the E2 roles. `journal` records keys and buttons injected into projected windows
    /// (separate from E1's); the returned outputs are its crash recovery, run first.
    pub fn new(
        config: &EngineConfig,
        journal: Box<dyn Journal>,
        now: MonoTime,
    ) -> Result<(E2, Vec<Output>), JournalError> {
        let mut out = Vec::new();
        let ledgers = Ledgers::new(journal, now, &mut out)?;
        Ok((
            E2 {
                node: config.node,
                state: SessionState {
                    lock: LockState::Locked,
                    active: None,
                },
                asleep: false,
                awaiting_state: true,
                panic: false,
                peers: BTreeSet::new(),
                grants: BTreeMap::new(),
                windows: BTreeMap::new(),
                scales: BTreeMap::new(),
                display_bounds: BTreeMap::new(),
                proxy_windows: BTreeMap::new(),
                focused: None,
                focus_before: None,
                next_projection: Some(1),
                sources: BTreeMap::new(),
                destinations: BTreeMap::new(),
                pending_parks: BTreeMap::new(),
                pending_places: BTreeMap::new(),
                ledgers,
                home: None,
            },
            out,
        ))
    }

    /// The controller's home (entering, home, exiting, or its bind not yet confirmed removed) or
    /// none. On `None → Some`: drain every source's ledger (`heartbeat(p, &[], ..)`: releases
    /// only, journaled) and start the filter of §2.4; on `Some → None`: lift it. Idempotent
    /// otherwise.
    pub(crate) fn set_home(
        &mut self,
        home: Option<(NodeId, ProjectionId)>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let was_home = self.home.is_some();
        self.home = home;
        if !was_home && home.is_some() {
            let projections: Vec<_> = self.sources.keys().copied().collect();
            for projection in projections {
                self.ledgers.heartbeat(projection, &[], now, out);
            }
        }
    }

    /// `Ledgers::settled()`: this node's E2 injectors hold nothing and owe no release.
    pub(crate) fn settled(&self) -> bool {
        self.ledgers.settled()
    }

    pub fn handle(&mut self, input: &Input, now: MonoTime, out: &mut Vec<Output>) {
        match input {
            Input::PeerUp { peer } => {
                self.peers.insert(*peer);
                self.resume_sources(*peer, now, out);
            }
            Input::LocalDisplays(displays) => {
                self.scales = displays.iter().map(|d| (d.id, d.geometry.scale)).collect();
                self.display_bounds = displays.iter().map(|d| (d.id, d.geometry)).collect();
            }
            Input::ProxyWindow { key, window } => {
                self.proxy_windows.insert(*window, *key);
            }
            Input::Grants(grants) => {
                self.grants = grants.clone();
                let sources: Vec<_> = self
                    .sources
                    .iter()
                    .filter(|(_, s)| !self.granted(s.peer, Capability::WindowShare))
                    .map(|(&id, _)| id)
                    .collect();
                let destinations: Vec<_> = self
                    .destinations
                    .keys()
                    .copied()
                    .filter(|key| !self.granted(key.source, Capability::WindowPresent))
                    .collect();
                for id in sources {
                    self.end_source(id, Reason::Revoked, false, now, out);
                }
                for key in destinations {
                    self.end_destination(key, Reason::Revoked, false, false, out);
                }
            }
            Input::Session(event) => {
                match event {
                    SessionEvent::State(state) => {
                        self.state = *state;
                        self.awaiting_state = false;
                    }
                    SessionEvent::WillSleep => self.asleep = true,
                    SessionEvent::Woke => {
                        self.asleep = false;
                        self.awaiting_state = true;
                    }
                    _ => {}
                }
                if !self.permits_io() {
                    self.end_all(Reason::Locked, now, out);
                }
            }
            Input::Windows(event) => match event {
                WindowEvent::Added(window) | WindowEvent::Changed(window) => {
                    self.window_changed(window, now, out);
                    self.windows.insert(window.id, window.clone());
                }
                WindowEvent::Removed(window) => {
                    self.windows.remove(window);
                    if self.focused == Some(*window) {
                        self.focused = None;
                    }
                    let ids: Vec<_> = self
                        .sources
                        .iter()
                        .filter(|(_, s)| s.window == *window)
                        .map(|(&id, _)| id)
                        .collect();
                    for id in ids {
                        self.end_source(id, Reason::WindowClosed, false, now, out);
                    }
                    self.pending_parks.remove(window);
                    self.pending_places.remove(window);
                }
                WindowEvent::Focused(window) => self.focused = *window,
                _ => {}
            },
            Input::Command(Command::Project { window, to, place }) => {
                let _ = self.project(*window, *to, now, out);
                if let Some(place) = place {
                    self.place_start(*place, 0, (0, 0), None, out);
                }
            }
            Input::Command(command @ (Command::Browse { .. } | Command::Pull { .. })) => {
                self.browse_command(*command, out);
            }
            Input::Command(Command::Return(key)) => {
                if key.source == self.node {
                    self.end_source(key.projection, Reason::Returned, false, now, out);
                } else {
                    self.end_destination(*key, Reason::Returned, false, false, out);
                }
            }
            Input::Command(Command::ReturnAt(key, place)) => self.return_at(*key, *place, now, out),
            Input::Command(Command::Panic) => {
                self.panic = true;
                self.end_all(Reason::Returned, now, out);
            }
            Input::Command(Command::Rearm) => self.panic = false,
            Input::Link(LinkEvent::Closed { peer, .. }) => {
                self.peers.remove(peer);
                let sources: Vec<_> = self
                    .sources
                    .iter()
                    .filter(|(_, s)| s.peer == *peer)
                    .map(|(&id, _)| id)
                    .collect();
                let destinations: Vec<_> = self
                    .destinations
                    .keys()
                    .copied()
                    .filter(|key| key.source == *peer)
                    .collect();
                for id in sources {
                    self.suspend_source(id, now, out);
                }
                for key in destinations {
                    self.suspend_destination(key, now);
                }
            }
            Input::Link(LinkEvent::Control {
                peer,
                msg: ControlMessage::Projection(msg),
            }) => {
                // Dispatch by wire direction, never by whichever map happens to match an id.
                match msg {
                    Message::Start { .. }
                    | Message::StartAt { .. }
                    | Message::Geometry { .. }
                    | Message::Title { .. }
                    | Message::End { .. }
                    | Message::WindowList { .. }
                    | Message::BrowseRefused { .. } => {
                        self.destination_control(*peer, msg, now, out)
                    }
                    Message::DragCancel { .. } => self.destination_control(*peer, msg, now, out),
                    Message::Accepted { .. }
                    | Message::Refused { .. }
                    | Message::Resize { .. }
                    | Message::Focus { .. }
                    | Message::KeyFrameRequest { .. }
                    | Message::Close { .. }
                    | Message::ListWindows { .. }
                    | Message::Pull { .. }
                    | Message::ProxyPlaced { .. } => self.source_control(*peer, msg, now, out),
                    Message::ReturnAt { projection, place }
                        if self
                            .sources
                            .get(projection)
                            .is_some_and(|s| s.peer == *peer) =>
                    {
                        self.return_at(
                            ProjectionKey {
                                source: self.node,
                                projection: *projection,
                            },
                            *place,
                            now,
                            out,
                        );
                    }
                    _ => {}
                }
            }
            Input::Link(LinkEvent::Input {
                peer,
                msg: InputMessage::Proj(msg),
            }) => self.source_input(*peer, msg, now, out),
            Input::Parked { window, result } => self.parked(*window, *result, now, out),
            Input::CaptureStarted { projection, result } => {
                self.capture_started(*projection, *result, now, out)
            }
            Input::CaptureEnded { stream, reason } => {
                self.capture_ended(*stream, *reason, now, out)
            }
            Input::ProxyOpened { key, result } => self.proxy_opened(*key, *result, now, out),
            Input::Proxy { key, event } => self.proxy_event(*key, event, now, out),
            Input::MediaError { key } => self.media_error(*key, now, out),
            Input::InjectDone { id, ok } => {
                for owner in self.ledgers.done(*id, *ok, now, out) {
                    self.end_source(owner, Reason::Failed, false, now, out);
                }
                self.source_inject_done(*id, *ok, now, out);
            }
            Input::Tick => {
                self.source_tick(now, out);
                self.ledgers.tick(now, out);
                self.destination_tick(now, out);
            }
            _ => {}
        }
    }

    pub fn next_deadline(&self) -> Option<MonoTime> {
        [
            self.source_deadline(),
            self.ledgers.next_deadline(),
            self.destination_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn permits_io(&self) -> bool {
        self.state.permits_io() && !self.asleep && !self.awaiting_state && !self.panic
    }

    fn granted(&self, peer: NodeId, capability: Capability) -> bool {
        self.grants
            .get(&peer)
            .is_some_and(|grants| grants.contains(&capability))
    }

    fn end_all(&mut self, reason: Reason, now: MonoTime, out: &mut Vec<Output>) {
        let sources: Vec<_> = self.sources.keys().copied().collect();
        let destinations: Vec<_> = self.destinations.keys().copied().collect();
        for id in sources {
            self.end_source(id, reason, false, now, out);
        }
        for key in destinations {
            self.end_destination(key, reason, false, false, out);
        }
    }
}

fn send(peer: NodeId, msg: Message, out: &mut Vec<Output>) {
    out.push(Output::SendControl {
        peer,
        msg: ControlMessage::Projection(msg),
    });
}
