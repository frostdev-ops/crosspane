//! E2 window projection roles: the source side (the window's owner) and the
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

/// What E2 knows that decides projection audio ("sound follows the window", AUD-PROJ). Derived
/// on demand from the live source and destination state ([`E2::audio_view`]), never stored, so
/// it cannot outlive the projections it describes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AudioView {
    /// pids (!= 0) of this node's projections in Capturing | Live | Restarting (accepted on
    /// this connection), per destination peer. A peer with no such projection has no entry.
    pub sources: BTreeMap<NodeId, BTreeSet<u32>>,
    /// peers with >= 1 projection shown here: open && !suspended
    pub destinations: BTreeSet<NodeId>,
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
    drag_in_peers: BTreeSet<NodeId>,
    next_browse_request: u32,
    automatic_pulls: BTreeMap<NodeId, (u32, WindowId, MonoTime)>,
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
    /// The audio-relevant state of both roles right now (AUD-PROJ).
    ///
    /// - `sources`: for each destination peer, the pids (never 0) of this node's projected
    ///   windows that peer has accepted on the current connection (source stages `Capturing`,
    ///   `Live` and `Restarting`). A projection that is offered, still being
    ///   parked, suspended by a link loss, or waiting for the destination to accept it again
    ///   after a resume (`Resuming`) is absent; so is an ended one.
    /// - `destinations`: the peers with at least one projection shown on this node (proxy open,
    ///   not suspended).
    pub(crate) fn audio_view(&self) -> AudioView {
        AudioView {
            sources: self.audio_sources(),
            destinations: self.audio_destinations(),
        }
    }

    /// A target reports only a proxy of its current controller's own projection.
    pub(crate) fn proxy_projection(
        &self,
        window: WindowId,
        controller: NodeId,
    ) -> Option<ProjectionId> {
        self.proxy_windows
            .get(&window)
            .filter(|key| key.source == controller)
            .map(|key| key.projection)
    }

    pub(crate) fn drag_in_available(&self, peer: NodeId) -> bool {
        self.peers.contains(&peer) && self.drag_in_peers.contains(&peer)
    }

    pub(crate) fn drag_in_home(
        &self,
        peer: NodeId,
        projection: ProjectionId,
    ) -> Option<ProjectionKey> {
        self.sources
            .get(&projection)
            .filter(|s| s.peer == peer)
            .map(|_| ProjectionKey {
                source: self.node,
                projection,
            })
    }

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
                drag_in_peers: BTreeSet::new(),
                next_browse_request: 1,
                automatic_pulls: BTreeMap::new(),
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
        self.automatic_pulls.retain(|_, (_, _, until)| now < *until);
        match input {
            Input::DragInPeer { peer, available } => {
                if *available {
                    self.drag_in_peers.insert(*peer);
                } else {
                    self.drag_in_peers.remove(peer);
                    self.automatic_pulls.remove(peer);
                }
            }
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
                }
                WindowEvent::Removed(window) => {
                    self.windows.remove(window);
                    self.stand_in_removed(*window, out);
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
                self.drag_in_peers.remove(peer);
                self.automatic_pulls.remove(peer);
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
                    | Message::PullAt { .. }
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
            self.automatic_pulls
                .values()
                .map(|(_, _, until)| *until)
                .min(),
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

#[cfg(test)]
mod audio_view_tests {
    //! `E2::audio_view` (AUD-PROJ): which of this node's projections send sound to which peer,
    //! and which peers' sound plays here, in every stage the two roles can be in.

    use crosspane_input::journal::MemoryJournal;
    use crosspane_platform::{
        Parked, ParkingKind as PlatformParking, StreamId, WindowRole, WindowState,
    };
    use crosspane_protocol::link::LinkError;
    use crosspane_protocol::projection::WindowSummary;
    use crosspane_types::geom::euclid::Point2D;
    use crosspane_types::geom::{PointLogical, RectLogical, SizeLogical};

    use super::*;
    use crate::io::{Failure, Notice, ProxyEvent};

    const A: NodeId = NodeId([1; 32]);
    const B: NodeId = NodeId([2; 32]);
    const C: NodeId = NodeId([3; 32]);
    const DISPLAY: DisplayId = DisplayId(4);
    const SCALE: f64 = 2.0;
    /// The content size every projection is accepted and parked at, unless a test says otherwise.
    const CONTENT: (i32, i32) = (640, 480);

    fn at(seconds: u64) -> MonoTime {
        MonoTime::from_nanos(seconds * 1_000_000_000)
    }

    fn feed(e2: &mut E2, input: Input, now: MonoTime) -> Vec<Output> {
        let mut out = Vec::new();
        e2.handle(&input, now, &mut out);
        out
    }

    fn control(peer: NodeId, msg: Message) -> Input {
        Input::Link(LinkEvent::Control {
            peer,
            msg: ControlMessage::Projection(msg),
        })
    }

    fn closed(peer: NodeId) -> Input {
        Input::Link(LinkEvent::Closed {
            peer,
            error: LinkError::Closed,
        })
    }

    fn session(lock: LockState) -> Input {
        Input::Session(SessionEvent::State(SessionState {
            lock,
            active: Some(true),
        }))
    }

    /// An unlocked node `A` with `peers` connected, each granted `grants`.
    fn node(peers: &[NodeId], grants: &[Capability]) -> E2 {
        let (mut e2, _) = E2::new(
            &EngineConfig::new(A),
            Box::new(MemoryJournal::default()),
            MonoTime::ZERO,
        )
        .unwrap();
        feed(&mut e2, session(LockState::Unlocked), MonoTime::ZERO);
        for &peer in peers {
            feed(&mut e2, Input::PeerUp { peer }, MonoTime::ZERO);
        }
        let grants: BTreeMap<NodeId, BTreeSet<Capability>> = peers
            .iter()
            .map(|&peer| (peer, grants.iter().copied().collect()))
            .collect();
        feed(&mut e2, Input::Grants(grants), MonoTime::ZERO);
        e2
    }

    fn view(sources: &[(NodeId, &[u32])], destinations: &[NodeId]) -> AudioView {
        AudioView {
            sources: sources
                .iter()
                .map(|&(peer, pids)| (peer, pids.iter().copied().collect()))
                .collect(),
            destinations: destinations.iter().copied().collect(),
        }
    }

    // ---- source side ----

    fn window_info(id: u64, pid: Option<u32>) -> WindowInfo {
        WindowInfo {
            id: WindowId(id),
            pid,
            display: Some(DISPLAY),
            title: "window".into(),
            app_id: "app".into(),
            role: WindowRole::Toplevel,
            parent: None,
            state: WindowState::Normal,
            frame: RectLogical::new(PointLogical::zero(), SizeLogical::new(320.0, 240.0)),
        }
    }

    fn add_window(e2: &mut E2, id: u64, pid: Option<u32>) {
        let info = window_info(id, pid);
        feed(e2, Input::Windows(WindowEvent::Added(info)), MonoTime::ZERO);
    }

    /// `Command::Project`: the window is offered to `peer`; returns the new projection's id.
    fn offer(e2: &mut E2, window: u64, peer: NodeId) -> ProjectionId {
        let out = feed(
            e2,
            Input::Command(Command::Project {
                window: WindowId(window),
                to: peer,
                place: None,
            }),
            MonoTime::ZERO,
        );
        out.iter()
            .find_map(|o| match o {
                Output::SendControl {
                    peer: to,
                    msg: ControlMessage::Projection(Message::Start { projection, .. }),
                } if *to == peer => Some(*projection),
                _ => None,
            })
            .expect("the window was offered")
    }

    fn accept(
        e2: &mut E2,
        peer: NodeId,
        projection: ProjectionId,
        size: (i32, i32),
        now: MonoTime,
    ) -> Vec<Output> {
        let accepted = Message::Accepted {
            projection,
            size: PixelSize::new(size.0 as u32, size.1 as u32),
            scale: SCALE,
        };
        feed(e2, control(peer, accepted), now)
    }

    fn park(e2: &mut E2, window: u64, size: (i32, i32), now: MonoTime) -> Vec<Output> {
        let parked = Parked {
            window: WindowId(window),
            display: DISPLAY,
            kind: PlatformParking::Twin,
            fullscreen: false,
            content: PixelRect::new(Point2D::new(20, 30), Point2D::new(20 + size.0, 30 + size.1)),
        };
        let result = Ok(parked);
        let input = Input::Parked {
            window: WindowId(window),
            result,
        };
        feed(e2, input, now)
    }

    fn capture_started(e2: &mut E2, projection: ProjectionId, now: MonoTime) -> Vec<Output> {
        let input = Input::CaptureStarted {
            projection,
            result: Ok(StreamId(projection.0)),
        };
        feed(e2, input, now)
    }

    /// Show `window` (process `pid`) to `peer` and drive it to `Live`.
    fn go_live(e2: &mut E2, window: u64, pid: Option<u32>, peer: NodeId) -> ProjectionId {
        add_window(e2, window, pid);
        let projection = offer(e2, window, peer);
        accept(e2, peer, projection, CONTENT, MonoTime::ZERO);
        park(e2, window, CONTENT, MonoTime::ZERO);
        capture_started(e2, projection, MonoTime::ZERO);
        projection
    }

    fn pids(e2: &E2, peer: NodeId) -> Vec<u32> {
        e2.audio_view()
            .sources
            .get(&peer)
            .map(|pids| pids.iter().copied().collect())
            .unwrap_or_default()
    }

    fn sends_start(out: &[Output], peer: NodeId) -> bool {
        out.iter().any(|o| {
            matches!(o, Output::SendControl {
                peer: to,
                msg: ControlMessage::Projection(Message::Start { .. }),
            } if *to == peer)
        })
    }

    #[test]
    fn a_pushed_projection_appears_once_accepted_and_parked() {
        let mut e2 = node(&[B], &[Capability::WindowShare]);
        assert_eq!(e2.audio_view(), AudioView::default());
        add_window(&mut e2, 10, Some(42));
        // Offered: the destination has not accepted it.
        let projection = offer(&mut e2, 10, B);
        assert_eq!(e2.audio_view(), AudioView::default());
        // Accepted, but the window is still being parked.
        accept(&mut e2, B, projection, CONTENT, MonoTime::ZERO);
        assert_eq!(e2.audio_view(), AudioView::default());
        // Parked: `ProjectionStarted` goes out and the sound follows it.
        let out = park(&mut e2, 10, CONTENT, MonoTime::ZERO);
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::Notice(Notice::ProjectionStarted { .. })))
        );
        assert_eq!(e2.audio_view(), view(&[(B, &[42])], &[]));
        // Live.
        capture_started(&mut e2, projection, MonoTime::ZERO);
        assert_eq!(e2.audio_view(), view(&[(B, &[42])], &[]));
    }

    #[test]
    fn a_refused_offer_never_appears() {
        let mut e2 = node(&[B], &[Capability::WindowShare]);
        add_window(&mut e2, 10, Some(42));
        let projection = offer(&mut e2, 10, B);
        let refused = Message::Refused {
            projection,
            reason: crosspane_protocol::msg::Refusal::Permission,
        };
        feed(&mut e2, control(B, refused), MonoTime::ZERO);
        assert_eq!(e2.audio_view(), AudioView::default());
    }

    #[test]
    fn a_pulled_projection_appears_like_a_pushed_one() {
        let mut e2 = node(&[B], &[Capability::WindowShare, Capability::WindowBrowse]);
        add_window(&mut e2, 10, Some(42));
        let pull = Message::Pull {
            request: 1,
            window: WindowId(10),
        };
        let out = feed(&mut e2, control(B, pull), MonoTime::ZERO);
        let projection = out
            .iter()
            .find_map(|o| match o {
                Output::SendControl {
                    msg: ControlMessage::Projection(Message::Start { projection, .. }),
                    ..
                } => Some(*projection),
                _ => None,
            })
            .expect("the pulled window was offered");
        assert_eq!(e2.audio_view(), AudioView::default());
        accept(&mut e2, B, projection, CONTENT, MonoTime::ZERO);
        assert_eq!(e2.audio_view(), AudioView::default());
        park(&mut e2, 10, CONTENT, MonoTime::ZERO);
        assert_eq!(e2.audio_view(), view(&[(B, &[42])], &[]));
    }

    #[test]
    fn a_resuming_projection_is_absent_until_accepted_again() {
        let mut e2 = node(&[B], &[Capability::WindowShare]);
        let projection = go_live(&mut e2, 10, Some(42), B);
        assert_eq!(pids(&e2, B), [42]);
        // Link loss: suspended for the reconnect grace.
        feed(&mut e2, closed(B), at(1));
        assert_eq!(e2.audio_view(), AudioView::default());
        // The peer is back and the window is offered again (`Resuming`): not accepted yet.
        let out = feed(&mut e2, Input::PeerUp { peer: B }, at(2));
        assert!(sends_start(&out, B), "{out:?}");
        assert_eq!(e2.audio_view(), AudioView::default());
        feed(&mut e2, Input::Tick, at(3));
        assert_eq!(e2.audio_view(), AudioView::default());
        // Accepted again, at another size: `Restarting` while the window is resized.
        let size = (800, 600);
        let out = accept(&mut e2, B, projection, size, at(4));
        assert!(
            out.iter().any(|o| matches!(o, Output::ResizeParked { .. })),
            "{out:?}"
        );
        assert_eq!(e2.audio_view(), view(&[(B, &[42])], &[]));
        // The restart finishes: capturing, then live.
        let out = park(&mut e2, 10, size, at(5));
        assert!(
            out.iter().any(|o| matches!(o, Output::StartCapture { .. })),
            "{out:?}"
        );
        assert_eq!(e2.audio_view(), view(&[(B, &[42])], &[]));
        capture_started(&mut e2, projection, at(5));
        assert_eq!(e2.audio_view(), view(&[(B, &[42])], &[]));
    }

    #[test]
    fn a_resume_that_is_accepted_at_the_same_size_goes_straight_to_capturing() {
        let mut e2 = node(&[B], &[Capability::WindowShare]);
        let projection = go_live(&mut e2, 10, Some(42), B);
        feed(&mut e2, closed(B), at(1));
        feed(&mut e2, Input::PeerUp { peer: B }, at(2));
        assert_eq!(e2.audio_view(), AudioView::default());
        let out = accept(&mut e2, B, projection, CONTENT, at(3));
        assert!(
            out.iter().any(|o| matches!(o, Output::StartCapture { .. })),
            "{out:?}"
        );
        assert_eq!(e2.audio_view(), view(&[(B, &[42])], &[]));
    }

    #[test]
    fn a_refused_or_expired_resume_never_appears() {
        // The destination refuses the re-offer.
        let mut e2 = node(&[B], &[Capability::WindowShare]);
        let projection = go_live(&mut e2, 10, Some(42), B);
        feed(&mut e2, closed(B), at(1));
        feed(&mut e2, Input::PeerUp { peer: B }, at(2));
        let refused = Message::Refused {
            projection,
            reason: crosspane_protocol::msg::Refusal::Permission,
        };
        feed(&mut e2, control(B, refused), at(3));
        assert_eq!(e2.audio_view(), AudioView::default());
        // The grace runs out while the link is still down.
        let mut e2 = node(&[B], &[Capability::WindowShare]);
        go_live(&mut e2, 10, Some(42), B);
        feed(&mut e2, closed(B), at(1));
        feed(&mut e2, Input::Tick, at(22));
        assert_eq!(e2.audio_view(), AudioView::default());
        // The grace runs out while the re-offer is unanswered.
        let mut e2 = node(&[B], &[Capability::WindowShare]);
        go_live(&mut e2, 10, Some(42), B);
        feed(&mut e2, closed(B), at(1));
        feed(&mut e2, Input::PeerUp { peer: B }, at(2));
        feed(&mut e2, Input::Tick, at(22));
        assert_eq!(e2.audio_view(), AudioView::default());
    }

    #[test]
    fn an_ended_projection_disappears() {
        let key = ProjectionKey {
            source: A,
            projection: ProjectionId(1),
        };
        let causes = [
            ("returned", Input::Command(Command::Return(key))),
            (
                "window closed",
                Input::Windows(WindowEvent::Removed(WindowId(10))),
            ),
            (
                "closed by the destination",
                control(
                    B,
                    Message::Close {
                        projection: ProjectionId(1),
                        reason: Reason::Returned,
                    },
                ),
            ),
            ("share revoked", Input::Grants(BTreeMap::new())),
            ("locked", session(LockState::Locked)),
            ("panic", Input::Command(Command::Panic)),
        ];
        for (name, cause) in causes {
            let mut e2 = node(&[B], &[Capability::WindowShare]);
            let projection = go_live(&mut e2, 10, Some(42), B);
            assert_eq!(projection, ProjectionId(1));
            assert_eq!(pids(&e2, B), [42], "{name}: before");
            feed(&mut e2, cause, at(1));
            assert_eq!(e2.audio_view(), AudioView::default(), "{name}: after");
        }
    }

    #[test]
    fn pid_zero_and_a_missing_pid_are_excluded() {
        let mut e2 = node(&[B], &[Capability::WindowShare]);
        go_live(&mut e2, 10, Some(0), B);
        assert_eq!(e2.audio_view(), AudioView::default());
        let mut e2 = node(&[B], &[Capability::WindowShare]);
        go_live(&mut e2, 10, None, B);
        assert_eq!(e2.audio_view(), AudioView::default());
        // The window's process is whatever the platform reports now.
        let changed = |pid| Input::Windows(WindowEvent::Changed(window_info(10, pid)));
        feed(&mut e2, changed(Some(7)), at(1));
        assert_eq!(e2.audio_view(), view(&[(B, &[7])], &[]));
        feed(&mut e2, changed(Some(0)), at(2));
        assert_eq!(e2.audio_view(), AudioView::default());
    }

    #[test]
    fn pids_are_deduplicated_per_peer_and_a_peer_stays_until_its_last_projection_ends() {
        let mut e2 = node(&[B, C], &[Capability::WindowShare]);
        go_live(&mut e2, 10, Some(42), B);
        go_live(&mut e2, 11, Some(42), B);
        go_live(&mut e2, 12, Some(43), B);
        go_live(&mut e2, 13, Some(42), C);
        assert_eq!(e2.audio_view(), view(&[(B, &[42, 43]), (C, &[42])], &[]));
        let end = |e2: &mut E2, projection| {
            let key = ProjectionKey {
                source: A,
                projection: ProjectionId(projection),
            };
            feed(e2, Input::Command(Command::Return(key)), at(1));
        };
        // A second window of the same process keeps the sound.
        end(&mut e2, 1);
        assert_eq!(e2.audio_view(), view(&[(B, &[42, 43]), (C, &[42])], &[]));
        end(&mut e2, 2);
        assert_eq!(e2.audio_view(), view(&[(B, &[43]), (C, &[42])], &[]));
        end(&mut e2, 3);
        assert_eq!(e2.audio_view(), view(&[(C, &[42])], &[]));
        end(&mut e2, 4);
        assert_eq!(e2.audio_view(), AudioView::default());
    }

    #[test]
    fn a_link_loss_suspends_only_that_peers_projections() {
        let mut e2 = node(&[B, C], &[Capability::WindowShare]);
        go_live(&mut e2, 10, Some(42), B);
        go_live(&mut e2, 11, Some(43), C);
        feed(&mut e2, closed(B), at(1));
        assert_eq!(e2.audio_view(), view(&[(C, &[43])], &[]));
    }

    // ---- destination side ----

    fn start(e2: &mut E2, peer: NodeId, projection: u64, now: MonoTime) -> Vec<Output> {
        let start = Message::Start {
            projection: ProjectionId(projection),
            window: WindowSummary {
                title: "t".into(),
                app_id: "a".into(),
            },
            size: PixelSize::new(400, 300),
        };
        feed(e2, control(peer, start), now)
    }

    fn open(e2: &mut E2, peer: NodeId, projection: u64, now: MonoTime) -> Vec<Output> {
        let input = Input::ProxyOpened {
            key: remote(peer, projection),
            result: Ok((PixelSize::new(400, 300), 1.0)),
        };
        feed(e2, input, now)
    }

    fn remote(source: NodeId, projection: u64) -> ProjectionKey {
        ProjectionKey {
            source,
            projection: ProjectionId(projection),
        }
    }

    fn sends_accepted(out: &[Output], peer: NodeId) -> bool {
        out.iter().any(|o| {
            matches!(o, Output::SendControl {
                peer: to,
                msg: ControlMessage::Projection(Message::Accepted { .. }),
            } if *to == peer)
        })
    }

    #[test]
    fn an_open_destination_appears() {
        let mut e2 = node(&[B], &[Capability::WindowPresent]);
        assert_eq!(e2.audio_view(), AudioView::default());
        // Start received, proxy still opening.
        let out = start(&mut e2, B, 1, at(1));
        assert!(out.iter().any(|o| matches!(o, Output::OpenProxy { .. })));
        assert_eq!(e2.audio_view(), AudioView::default());
        // The proxy opened and the projection was accepted.
        let out = open(&mut e2, B, 1, at(1));
        assert!(sends_accepted(&out, B), "{out:?}");
        assert_eq!(e2.audio_view(), view(&[], &[B]));
    }

    #[test]
    fn a_destination_that_failed_to_open_never_appears() {
        let mut e2 = node(&[B], &[Capability::WindowPresent]);
        start(&mut e2, B, 1, at(1));
        let failed = Input::ProxyOpened {
            key: remote(B, 1),
            result: Err(Failure::Other),
        };
        feed(&mut e2, failed, at(1));
        assert_eq!(e2.audio_view(), AudioView::default());
        // Without the present grant the projection is refused outright.
        let mut e2 = node(&[B], &[]);
        let out = start(&mut e2, B, 1, at(1));
        assert!(!out.iter().any(|o| matches!(o, Output::OpenProxy { .. })));
        assert_eq!(e2.audio_view(), AudioView::default());
    }

    #[test]
    fn a_suspended_destination_is_absent_until_the_resume() {
        let mut e2 = node(&[B], &[Capability::WindowPresent]);
        start(&mut e2, B, 1, at(1));
        open(&mut e2, B, 1, at(1));
        assert_eq!(e2.audio_view(), view(&[], &[B]));
        // Link loss suspends it (the proxy stays for the grace).
        feed(&mut e2, closed(B), at(2));
        assert_eq!(e2.destinations.len(), 1);
        assert_eq!(e2.audio_view(), AudioView::default());
        // The peer is back, but the source has not re-offered yet.
        feed(&mut e2, Input::PeerUp { peer: B }, at(3));
        assert_eq!(e2.audio_view(), AudioView::default());
        // The re-offer un-suspends it before `Accepted` goes out.
        let out = start(&mut e2, B, 1, at(4));
        assert!(sends_accepted(&out, B), "{out:?}");
        assert_eq!(e2.audio_view(), view(&[], &[B]));
    }

    #[test]
    fn a_proxy_that_opens_during_a_link_loss_shows_only_after_the_resume() {
        let mut e2 = node(&[B], &[Capability::WindowPresent]);
        start(&mut e2, B, 1, at(1));
        feed(&mut e2, closed(B), at(2));
        // The open completes while suspended: open, but not shown.
        let out = open(&mut e2, B, 1, at(3));
        assert!(!sends_accepted(&out, B), "{out:?}");
        assert_eq!(e2.audio_view(), AudioView::default());
        feed(&mut e2, Input::PeerUp { peer: B }, at(4));
        assert_eq!(e2.audio_view(), AudioView::default());
        let out = start(&mut e2, B, 1, at(5));
        assert!(sends_accepted(&out, B), "{out:?}");
        assert_eq!(e2.audio_view(), view(&[], &[B]));
        // Opening is not enough on its own either: a resume whose proxy is still opening.
        let mut e2 = node(&[B], &[Capability::WindowPresent]);
        start(&mut e2, B, 1, at(1));
        feed(&mut e2, closed(B), at(2));
        feed(&mut e2, Input::PeerUp { peer: B }, at(3));
        start(&mut e2, B, 1, at(4));
        assert_eq!(e2.audio_view(), AudioView::default());
        open(&mut e2, B, 1, at(5));
        assert_eq!(e2.audio_view(), view(&[], &[B]));
    }

    #[test]
    fn an_ended_destination_disappears() {
        let causes = [
            (
                "ended by the source",
                control(
                    B,
                    Message::End {
                        projection: ProjectionId(1),
                        reason: Reason::WindowClosed,
                    },
                ),
            ),
            ("returned", Input::Command(Command::Return(remote(B, 1)))),
            (
                "proxy closed",
                Input::Proxy {
                    key: remote(B, 1),
                    event: ProxyEvent::CloseRequested,
                },
            ),
            (
                "proxy lost",
                Input::Proxy {
                    key: remote(B, 1),
                    event: ProxyEvent::Lost,
                },
            ),
            ("present revoked", Input::Grants(BTreeMap::new())),
            ("locked", session(LockState::Locked)),
            ("panic", Input::Command(Command::Panic)),
        ];
        for (name, cause) in causes {
            let mut e2 = node(&[B], &[Capability::WindowPresent]);
            start(&mut e2, B, 1, at(1));
            open(&mut e2, B, 1, at(1));
            assert_eq!(e2.audio_view(), view(&[], &[B]), "{name}: before");
            feed(&mut e2, cause, at(2));
            assert_eq!(e2.audio_view(), AudioView::default(), "{name}: after");
        }
    }

    #[test]
    fn a_destination_whose_grace_ran_out_is_gone_and_a_new_one_starts_unshown() {
        let mut e2 = node(&[B], &[Capability::WindowPresent]);
        start(&mut e2, B, 1, at(1));
        open(&mut e2, B, 1, at(1));
        feed(&mut e2, closed(B), at(2));
        feed(&mut e2, Input::Tick, at(23));
        assert!(e2.destinations.is_empty());
        assert_eq!(e2.audio_view(), AudioView::default());
        feed(&mut e2, Input::PeerUp { peer: B }, at(24));
        start(&mut e2, B, 1, at(25));
        assert_eq!(e2.audio_view(), AudioView::default());
        open(&mut e2, B, 1, at(25));
        assert_eq!(e2.audio_view(), view(&[], &[B]));
    }

    #[test]
    fn a_peer_stays_a_destination_until_its_last_projection_ends() {
        let mut e2 = node(&[B, C], &[Capability::WindowPresent]);
        for (peer, projection) in [(B, 1), (B, 2), (C, 1)] {
            start(&mut e2, peer, projection, at(1));
            open(&mut e2, peer, projection, at(1));
        }
        assert_eq!(e2.audio_view(), view(&[], &[B, C]));
        let ret = |e2: &mut E2, peer, projection| {
            let input = Input::Command(Command::Return(remote(peer, projection)));
            feed(e2, input, at(2));
        };
        ret(&mut e2, B, 1);
        assert_eq!(e2.audio_view(), view(&[], &[B, C]));
        ret(&mut e2, B, 2);
        assert_eq!(e2.audio_view(), view(&[], &[C]));
        // A link loss suspends every projection of that peer at once.
        feed(&mut e2, closed(C), at(3));
        assert_eq!(e2.audio_view(), AudioView::default());
    }

    #[test]
    fn the_two_roles_are_reported_independently() {
        let grants = [Capability::WindowShare, Capability::WindowPresent];
        let mut e2 = node(&[B], &grants);
        go_live(&mut e2, 10, Some(42), B);
        start(&mut e2, B, 77, at(1));
        assert_eq!(e2.audio_view(), view(&[(B, &[42])], &[]));
        open(&mut e2, B, 77, at(1));
        assert_eq!(e2.audio_view(), view(&[(B, &[42])], &[B]));
        // Ending the source projection leaves the destination, and the other way round.
        let key = ProjectionKey {
            source: A,
            projection: ProjectionId(1),
        };
        feed(&mut e2, Input::Command(Command::Return(key)), at(2));
        assert_eq!(e2.audio_view(), view(&[], &[B]));
        go_live(&mut e2, 11, Some(43), B);
        assert_eq!(e2.audio_view(), view(&[(B, &[43])], &[B]));
        feed(
            &mut e2,
            Input::Command(Command::Return(remote(B, 77))),
            at(3),
        );
        assert_eq!(e2.audio_view(), view(&[(B, &[43])], &[]));
        // A link loss suspends both roles at once.
        feed(&mut e2, closed(B), at(4));
        assert_eq!(e2.audio_view(), AudioView::default());
    }
}
