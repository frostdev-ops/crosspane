//! The deterministic controller role: portals, capture fences and remote input sessions.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crosspane_input::Held;
use crosspane_input::accel::Accelerator;
use crosspane_input::layout::{Layout, Placed, PointerTracker, Step};
use crosspane_input::lease::ControllerLease;
use crosspane_input::router::Router;
use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, HotkeyEvent, LockState, MotionKind, Overlay,
    OverlayAnchor, OverlayEvent, PortalId, Rgb8, SessionEvent, SessionState,
};
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{
    ControlMessage, EndReason, InputMessage, MAX_HELD_KEYS, Placement, PointerMessage, TargetStatus,
};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::PointDevice;
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, GlobalDisplayId, NodeId, SessionId};
use crosspane_types::input::LockKeys;
use crosspane_types::time::MonoTime;

use crate::config::EngineConfig;
use crate::io::{Command, HUD, Input, Notice, Output};

const HUD_TIMEOUT: Duration = Duration::from_millis(500);
const START_TIMEOUT: Duration = Duration::from_secs(1);
const END_TIMEOUT: Duration = Duration::from_millis(300);
const NOT_PERMITTED: SessionState = SessionState {
    lock: LockState::Unknown,
    active: None,
};

#[derive(Debug)]
struct Session {
    peer: NodeId,
    id: SessionId,
    input_seq: u32,
    motion_seq: u32,
    lease: ControllerLease,
}

impl Session {
    fn transition(&mut self, item: Held, down: bool, now: MonoTime, out: &mut Vec<Output>) {
        let (session, seq) = self.next_input(now);
        let msg = match item {
            Held::Key(usage) => InputMessage::Key {
                session,
                seq,
                usage,
                down,
            },
            Held::Button(button) => InputMessage::Button {
                session,
                seq,
                button,
                down,
            },
        };
        out.push(Output::SendInput {
            peer: self.peer,
            msg,
        });
    }

    fn next_input(&mut self, now: MonoTime) -> (SessionId, u32) {
        let seq = self.input_seq;
        self.input_seq = seq.saturating_add(1);
        self.lease.sent(seq, now);
        (self.id, seq)
    }

    // Reserve room for every possible held item and its final release before accepting more
    // input. There are at most MAX_HELD_KEYS keys and 256 distinct MouseButton values.
    fn has_sequence_room(&self) -> bool {
        self.input_seq < u32::MAX - 290 && self.motion_seq < u32::MAX
    }
}

#[derive(Clone, Copy, Debug)]
struct Capture {
    id: CaptureId,
    started: bool,
}

#[derive(Clone, Copy, Debug)]
enum Wait {
    Hud(MonoTime),
    Handshake(MonoTime),
    Capture(MonoTime),
}

#[derive(Debug)]
struct Crossing {
    portal: PortalId,
    hud_display: DisplayId,
    entry: (GlobalDisplayId, PointDevice),
    session: Option<Session>,
    // Some during activation or a third-node handshake that keeps the capture.
    capture: Option<Capture>,
    wait: Wait,
}

#[derive(Debug)]
struct Control {
    session: Session,
    capture: Capture,
    tracker: PointerTracker,
    hud_display: DisplayId,
}

#[derive(Debug)]
enum Phase {
    Idle,
    Crossing(Crossing),
    Controlling(Control),
    Returning {
        capture: CaptureId,
        deadline: MonoTime,
    },
}

#[derive(Clone, Copy, Debug)]
struct Push {
    portal: PortalId,
    position: f64,
    since: MonoTime,
}

#[derive(Debug)]
struct HotkeyHold {
    since: MonoTime,
    fired: bool,
    rearm: bool,
}

/// The controller side of E1: crossing, capture, routing, heartbeats, release and panic.
#[derive(Debug)]
pub struct ControllerE1 {
    config: EngineConfig,
    state: SessionState,
    asleep: bool,
    displays: BTreeMap<NodeId, Vec<DisplayInfo>>,
    placements: Vec<Placement>,
    peers: BTreeSet<NodeId>,
    rtts: BTreeMap<NodeId, Duration>,
    lock_keys: LockKeys,
    armed: bool,
    // The authoritative hotkey pair for a chord already handled in captured Key events.
    chord_press_outstanding: bool,
    layout: Option<Layout>,
    portals: Vec<CapturePortal>,
    accelerator: Accelerator,
    router: Router,
    chord_keys: BTreeSet<HidUsage>,
    // Includes downs after Started but before activation completes, which aren't routed.
    capture_buttons: BTreeSet<MouseButton>,
    phase: Phase,
    push: Option<Push>,
    hotkey: Option<HotkeyHold>,
    next_session: Option<u64>,
    next_capture: Option<u64>,
}

impl ControllerE1 {
    pub fn new(config: &EngineConfig, now: MonoTime) -> ControllerE1 {
        let _ = now;
        ControllerE1 {
            config: config.clone(),
            state: NOT_PERMITTED,
            asleep: false,
            displays: BTreeMap::new(),
            placements: Vec::new(),
            peers: BTreeSet::new(),
            rtts: BTreeMap::new(),
            lock_keys: LockKeys::default(),
            armed: true,
            chord_press_outstanding: false,
            layout: None,
            portals: Vec::new(),
            accelerator: Accelerator::new(config.accel),
            router: Router::new(),
            chord_keys: BTreeSet::new(),
            capture_buttons: BTreeSet::new(),
            phase: Phase::Idle,
            push: None,
            hotkey: None,
            next_session: Some(1),
            next_capture: Some(1),
        }
    }

    /// Handle one input (every input is offered to both roles), appending outputs.
    pub fn handle(&mut self, input: &Input, now: MonoTime, out: &mut Vec<Output>) {
        match input {
            Input::LocalDisplays(displays) => {
                self.displays.insert(self.config.node, displays.clone());
                self.rebuild_layout(now, out);
            }
            Input::PeerDisplays { peer, displays } if *peer != self.config.node => {
                self.displays.insert(*peer, displays.clone());
                self.rebuild_layout(now, out);
            }
            Input::Layout(placements) => {
                self.placements = placements.clone();
                self.rebuild_layout(now, out);
            }
            Input::PeerUp { peer } => {
                self.peers.insert(*peer);
                self.update_portals(out);
            }
            Input::PeerRtt { peer, rtt } => {
                self.rtts.insert(*peer, *rtt);
            }
            Input::Session(event) => {
                match event {
                    SessionEvent::State(state) => self.state = *state,
                    SessionEvent::WillSleep => self.asleep = true,
                    SessionEvent::Woke => {
                        self.asleep = false;
                        self.state = NOT_PERMITTED;
                    }
                    _ => {}
                }
                if !self.permits_io() {
                    self.return_home(EndReason::ControllerLocked, None, false, true, now, out);
                }
            }
            Input::Capture(event) => self.capture_event(event, now, out),
            Input::CaptureBegun { id, result } => {
                let matches = matches!(&self.phase, Phase::Crossing(c)
                    if matches!(c.wait, Wait::Capture(_)) && c.capture.is_some_and(|v| v.id == *id));
                if matches {
                    match result {
                        Ok(start) => {
                            if matches!(&self.phase, Phase::Crossing(c)
                                if matches!(c.wait, Wait::Capture(deadline) if now >= deadline))
                            {
                                self.return_home(EndReason::Released, None, false, true, now, out);
                            } else {
                                self.chord_keys.extend(start.held_keys.iter().copied());
                                self.lock_keys = start.lock_keys;
                                self.activate(now, out);
                            }
                        }
                        Err(_) => self.return_home(EndReason::Released, None, true, true, now, out),
                    }
                } else if result.is_ok() {
                    // A delayed or duplicate success must never leave an unseen capture alive.
                    out.push(Output::EndCapture { warp_to: None });
                } else if result.is_err()
                    && matches!(self.phase, Phase::Returning { capture, .. } if capture == *id)
                {
                    // A rolled-back activation cannot emit an Ended fence.
                    self.finish_return(out);
                }
            }
            Input::Overlay(OverlayEvent::Visible(id)) if *id == HUD => {
                if let Phase::Crossing(c) = &self.phase
                    && let Wait::Hud(deadline) = c.wait
                {
                    if now >= deadline {
                        self.return_home(EndReason::Released, None, false, true, now, out);
                    } else {
                        self.start_handshake(now, out);
                    }
                }
            }
            Input::Overlay(OverlayEvent::Unavailable(id)) if *id == HUD => {
                self.return_home(EndReason::Released, None, false, true, now, out);
            }
            Input::Link(event) => self.link_event(event, now, out),
            Input::Hotkey(event) => self.hotkey_event(*event, now, out),
            Input::Command(Command::ReleaseControl) if !matches!(self.phase, Phase::Idle) => {
                self.release(now, out);
            }
            Input::Command(Command::Panic) => self.panic(now, out),
            Input::Command(Command::Rearm) => self.arm(out),
            Input::Tick => self.tick(now, out),
            _ => {}
        }
    }

    pub fn next_deadline(&self) -> Option<MonoTime> {
        let phase = match &self.phase {
            Phase::Idle => self
                .push
                .map(|p| p.since.saturating_add(self.config.push_to_cross)),
            Phase::Crossing(c) => match c.wait {
                Wait::Hud(at) | Wait::Handshake(at) | Wait::Capture(at) => Some(at),
            },
            Phase::Controlling(c) => {
                let heartbeat = c
                    .session
                    .lease
                    .next_heartbeat(!self.router.held_on(c.session.peer).is_empty());
                let ack = c
                    .session
                    .lease
                    .ack_deadline(self.rtts.get(&c.session.peer).copied());
                Some(ack.map_or(heartbeat, |at| heartbeat.min(at)))
            }
            Phase::Returning { deadline, .. } => Some(*deadline),
        };
        let panic = self
            .hotkey
            .as_ref()
            .filter(|h| !h.fired)
            .map(|h| h.since.saturating_add(self.config.panic_hold));
        match (phase, panic) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn permits_io(&self) -> bool {
        self.state.permits_io() && !self.asleep
    }

    fn session(&self) -> Option<&Session> {
        match &self.phase {
            Phase::Crossing(c) => c.session.as_ref(),
            Phase::Controlling(c) => Some(&c.session),
            _ => None,
        }
    }

    fn capture_mut(&mut self) -> Option<&mut Capture> {
        match &mut self.phase {
            Phase::Crossing(c) => c.capture.as_mut(),
            Phase::Controlling(c) => Some(&mut c.capture),
            _ => None,
        }
    }

    fn rebuild_layout(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let placed = self
            .placements
            .iter()
            .filter_map(|p| {
                let display = self
                    .displays
                    .get(&p.node)?
                    .iter()
                    .find(|d| d.id == p.display)?;
                Some(Placed {
                    id: GlobalDisplayId {
                        node: p.node,
                        display: p.display,
                    },
                    geometry: display.geometry,
                    origin: p.origin,
                })
            })
            .collect();
        self.layout = Layout::new(placed, self.config.layout).ok();
        self.update_portals(out);
        // Hot-plug must not leave a tracker or a pending capture on a vanished display.
        let valid = match (&self.phase, &self.layout) {
            (Phase::Crossing(c), Some(layout)) => {
                layout.get(c.entry.0).is_some()
                    && layout
                        .get(GlobalDisplayId {
                            node: self.config.node,
                            display: c.hud_display,
                        })
                        .is_some()
                    && ((c.capture.is_some() && matches!(c.wait, Wait::Handshake(_)))
                        || layout.portals().iter().any(|p| {
                            p.id == c.portal
                                && p.from.node == self.config.node
                                && p.from.display == c.hud_display
                                && p.to == c.entry.0
                        }))
            }
            (Phase::Controlling(c), Some(layout)) => {
                layout.get(c.tracker.position().0).is_some()
                    && layout
                        .get(GlobalDisplayId {
                            node: self.config.node,
                            display: c.hud_display,
                        })
                        .is_some()
            }
            (Phase::Crossing(_) | Phase::Controlling(_), None) => false,
            _ => true,
        };
        if !valid {
            self.return_home(EndReason::Released, None, false, true, now, out);
        }
        if self
            .push
            .is_some_and(|p| self.portal_entry(p.portal, p.position).is_none())
        {
            self.push = None;
        }
    }

    fn update_portals(&mut self, out: &mut Vec<Output>) {
        let portals = if self.armed {
            self.layout.as_ref().map_or_else(Vec::new, |layout| {
                layout
                    .capture_portals(self.config.node)
                    .into_iter()
                    .filter(|p| {
                        layout
                            .portals()
                            .iter()
                            .any(|v| v.id == p.id && self.peers.contains(&v.to.node))
                    })
                    .collect()
            })
        } else {
            Vec::new()
        };
        if portals != self.portals {
            self.portals = portals;
            out.push(Output::SetPortals(self.portals.clone()));
        }
    }

    fn portal_entry(
        &self,
        portal: PortalId,
        position: f64,
    ) -> Option<(DisplayId, GlobalDisplayId, PointDevice)> {
        if !self.armed || !self.permits_io() || !self.router.no_buttons_held() {
            return None;
        }
        let layout = self.layout.as_ref()?;
        let p = layout.portals().iter().find(|p| {
            p.id == portal && p.from.node == self.config.node && self.peers.contains(&p.to.node)
        })?;
        let (display, entry) = layout.entry(portal, position)?;
        Some((p.from.display, display, entry))
    }

    fn begin_crossing(&mut self, push: Push, now: MonoTime, out: &mut Vec<Output>) {
        self.push = None;
        let Some((hud_display, display, entry)) = self.portal_entry(push.portal, push.position)
        else {
            return;
        };
        self.show_hud(hud_display, display.node, out);
        self.phase = Phase::Crossing(Crossing {
            portal: push.portal,
            hud_display,
            entry: (display, entry),
            session: None,
            capture: None,
            wait: Wait::Hud(now.saturating_add(HUD_TIMEOUT)),
        });
    }

    fn show_hud(&self, display: DisplayId, peer: NodeId, out: &mut Vec<Output>) {
        out.push(Output::ShowOverlay {
            id: HUD,
            overlay: Overlay {
                display,
                anchor: OverlayAnchor::TopCenter,
                text: format!("Input → {}", peer.short()),
                accent: Rgb8 {
                    r: 0x3b,
                    g: 0x82,
                    b: 0xf6,
                },
            },
        });
    }

    fn start_handshake(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let Some(id) = self.next_session else {
            self.return_home(EndReason::Released, None, false, true, now, out);
            return;
        };
        let Phase::Crossing(c) = &mut self.phase else {
            return;
        };
        self.next_session = id.checked_add(1);
        let session = Session {
            peer: c.entry.0.node,
            id: SessionId(id),
            input_seq: 1,
            motion_seq: 1,
            lease: ControllerLease::new(now),
        };
        out.push(Output::SendControl {
            peer: session.peer,
            msg: ControlMessage::StartControl {
                session: session.id,
                entry_display: c.entry.0.display,
                entry: c.entry.1,
                lock_keys: self.lock_keys,
            },
        });
        c.session = Some(session);
        c.wait = Wait::Handshake(now.saturating_add(START_TIMEOUT));
    }

    fn activate(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        if let Phase::Crossing(c) = phase {
            let tracker = self
                .layout
                .as_ref()
                .and_then(|layout| PointerTracker::new(layout, c.entry.0, c.entry.1));
            match (tracker, c) {
                (
                    Some(tracker),
                    Crossing {
                        session: Some(mut session),
                        capture: Some(capture),
                        hud_display,
                        ..
                    },
                ) => {
                    session.lease = ControllerLease::new(now);
                    self.accelerator = Accelerator::new(self.config.accel);
                    self.phase = Phase::Controlling(Control {
                        session,
                        capture,
                        tracker,
                        hud_display,
                    });
                }
                (_, crossing) => {
                    self.phase = Phase::Crossing(crossing);
                    self.return_home(EndReason::Released, None, false, true, now, out);
                }
            }
        } else {
            self.phase = phase;
        }
    }

    fn capture_event(&mut self, event: &CaptureEvent, now: MonoTime, out: &mut Vec<Output>) {
        match event {
            CaptureEvent::LockKeys(keys) => self.lock_keys = *keys,
            CaptureEvent::EdgePressed {
                portal,
                position,
                at,
            } if matches!(self.phase, Phase::Idle) => {
                if self.portal_entry(*portal, *position).is_none() {
                    self.push = None;
                    return;
                }
                let push = match self.push {
                    Some(p) if p.portal == *portal => Push {
                        position: *position,
                        ..p
                    },
                    _ => Push {
                        portal: *portal,
                        position: *position,
                        since: *at,
                    },
                };
                if at.saturating_duration_since(push.since) >= self.config.push_to_cross {
                    self.begin_crossing(push, now, out);
                } else {
                    self.push = Some(push);
                }
            }
            CaptureEvent::EdgeReleased { portal, .. } => {
                if self.push.is_some_and(|p| p.portal == *portal) {
                    self.push = None;
                }
            }
            CaptureEvent::Started { id } => {
                if let Some(capture) = self.capture_mut().filter(|c| c.id == *id) {
                    capture.started = true;
                }
            }
            CaptureEvent::Ended { id, .. } => {
                if matches!(self.phase, Phase::Returning { capture, .. } if capture == *id) {
                    self.finish_return(out);
                } else if self.capture_mut().is_some_and(|c| c.id == *id) {
                    self.return_home(EndReason::Released, None, true, true, now, out);
                }
            }
            CaptureEvent::Key { usage, down, .. }
                if self.capture_mut().is_some_and(|c| c.started) =>
            {
                if *down {
                    self.chord_keys.insert(*usage);
                } else {
                    self.chord_keys.remove(usage);
                }
                if *down
                    && *usage == self.config.release_chord.key
                    && self
                        .config
                        .release_chord
                        .modifiers
                        .iter()
                        .all(|k| self.chord_keys.contains(k))
                {
                    // The authoritative hotkey pair may arrive after this captured chord.
                    // Suppress that pair for re-arm purposes without comparing timestamps.
                    self.chord_press_outstanding = true;
                    self.release(now, out);
                } else {
                    self.route(Held::Key(*usage), *down, now, out);
                }
            }
            CaptureEvent::Button { button, down, .. }
                if self.capture_mut().is_some_and(|c| c.started) =>
            {
                if *down {
                    self.capture_buttons.insert(*button);
                } else {
                    self.capture_buttons.remove(button);
                }
                self.route(Held::Button(*button), *down, now, out);
            }
            CaptureEvent::Scroll { delta, .. } if self.capture_mut().is_some_and(|c| c.started) => {
                if self.ensure_sequence_room(now, out)
                    && let Phase::Controlling(c) = &mut self.phase
                {
                    let (session, seq) = c.session.next_input(now);
                    out.push(Output::SendInput {
                        peer: c.session.peer,
                        msg: InputMessage::Scroll {
                            session,
                            seq,
                            delta: *delta,
                        },
                    });
                }
            }
            CaptureEvent::Motion { dx, dy, kind, at }
                if self.capture_mut().is_some_and(|c| c.started) =>
            {
                self.motion(*dx, *dy, *kind, *at, now, out);
            }
            // KeyboardBlinded is followed by Ended by the backend; it isn't itself a fence.
            _ => {}
        }
    }

    fn ensure_sequence_room(&mut self, now: MonoTime, out: &mut Vec<Output>) -> bool {
        if matches!(&self.phase, Phase::Controlling(c) if !c.session.has_sequence_room()) {
            self.return_home(EndReason::Released, None, false, true, now, out);
            false
        } else {
            matches!(self.phase, Phase::Controlling(_))
        }
    }

    fn route(&mut self, item: Held, down: bool, now: MonoTime, out: &mut Vec<Output>) {
        if !self.ensure_sequence_room(now, out) {
            return;
        }
        let Phase::Controlling(c) = &mut self.phase else {
            return;
        };
        if let Some(peer) = self.router.route(item, down, c.session.peer)
            && peer == c.session.peer
        {
            c.session.transition(item, down, now, out);
            if down
                && matches!(item, Held::Key(_))
                && self
                    .router
                    .held_on(peer)
                    .iter()
                    .filter(|h| matches!(h, Held::Key(_)))
                    .count()
                    > MAX_HELD_KEYS
            {
                // Release the excess key rather than silently omitting a held key from State.
                self.router.route(item, false, peer);
                c.session.transition(item, false, now, out);
            }
        }
    }

    fn motion(
        &mut self,
        dx: f64,
        dy: f64,
        kind: MotionKind,
        at: MonoTime,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if !self.ensure_sequence_room(now, out) {
            return;
        }
        let mm = match kind {
            MotionKind::Unaccelerated => self.accelerator.unaccelerated(dx, dy, at),
            MotionKind::Accelerated { display } => {
                let Some(info) = self
                    .displays
                    .get(&self.config.node)
                    .and_then(|ds| ds.iter().find(|d| d.id == display))
                else {
                    return;
                };
                self.accelerator.accelerated(dx, dy, &info.geometry)
            }
        };
        let (Some(layout), Phase::Controlling(c)) = (&self.layout, &mut self.phase) else {
            return;
        };
        let previous = c.tracker.position();
        match c.tracker.step(layout, mm) {
            Step::On { display, position } => {
                let seq = c.session.motion_seq;
                c.session.motion_seq = seq.saturating_add(1);
                out.push(Output::SendMotion {
                    peer: c.session.peer,
                    msg: PointerMessage {
                        session: c.session.id,
                        seq,
                        display: display.display,
                        position,
                    },
                });
            }
            Step::Crossed {
                display,
                position,
                portal,
            } => {
                if !self.router.no_buttons_held()
                    || !self.capture_buttons.is_empty()
                    || (display.node != self.config.node && !self.peers.contains(&display.node))
                {
                    if let Some(tracker) = PointerTracker::new(layout, previous.0, previous.1) {
                        c.tracker = tracker;
                    }
                } else if display.node == self.config.node {
                    self.return_home(
                        EndReason::Released,
                        Some((display.display, position)),
                        false,
                        true,
                        now,
                        out,
                    );
                } else {
                    self.switch_target(portal, (display, position), now, out);
                }
            }
        }
    }

    fn switch_target(
        &mut self,
        portal: PortalId,
        entry: (GlobalDisplayId, PointDevice),
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        if let Phase::Controlling(mut c) = phase {
            self.end_session(&mut c.session, EndReason::Released, true, now, out);
            self.phase = Phase::Crossing(Crossing {
                portal,
                hud_display: c.hud_display,
                entry,
                session: None,
                capture: Some(c.capture),
                wait: Wait::Handshake(now.saturating_add(START_TIMEOUT)),
            });
            self.start_handshake(now, out);
            if matches!(self.phase, Phase::Crossing(_)) {
                self.show_hud(c.hud_display, entry.0.node, out);
            }
        } else {
            self.phase = phase;
        }
    }

    fn link_event(&mut self, event: &LinkEvent, now: MonoTime, out: &mut Vec<Output>) {
        if let LinkEvent::Closed { peer, .. } = event {
            self.peers.remove(peer);
            self.rtts.remove(peer);
            self.update_portals(out);
            if self.session().is_some_and(|s| s.peer == *peer)
                || matches!(&self.phase, Phase::Crossing(c) if c.entry.0.node == *peer)
            {
                out.push(Output::Notice(Notice::LostConnection(*peer)));
                self.return_home(EndReason::LinkLost, None, false, false, now, out);
            }
            if self
                .push
                .is_some_and(|p| self.portal_entry(p.portal, p.position).is_none())
            {
                self.push = None;
            }
            return;
        }
        match event {
            LinkEvent::Control {
                peer,
                msg: ControlMessage::ControlStarted { session },
            } if self
                .session()
                .is_some_and(|s| s.peer == *peer && s.id == *session) =>
            {
                let Phase::Crossing(c) = &mut self.phase else {
                    return;
                };
                let Wait::Handshake(deadline) = c.wait else {
                    return;
                };
                if now >= deadline {
                    out.push(Output::Notice(Notice::LostConnection(*peer)));
                    self.return_home(EndReason::LinkLost, None, false, true, now, out);
                    return;
                }
                if c.capture.is_some() {
                    self.activate(now, out);
                } else if let Some(id) = self.next_capture {
                    self.next_capture = id.checked_add(1);
                    let capture = Capture {
                        id: CaptureId(id),
                        started: false,
                    };
                    out.push(Output::BeginCapture {
                        id: capture.id,
                        portal: c.portal,
                    });
                    c.capture = Some(capture);
                    c.wait = Wait::Capture(now.saturating_add(START_TIMEOUT));
                } else {
                    self.return_home(EndReason::Released, None, false, true, now, out);
                }
            }
            LinkEvent::Control {
                peer,
                msg: ControlMessage::ControlRefused { session, reason },
            } if matches!(&self.phase, Phase::Crossing(c) if matches!(c.wait, Wait::Handshake(_)))
                && self
                    .session()
                    .is_some_and(|s| s.peer == *peer && s.id == *session) =>
            {
                out.push(Output::Notice(Notice::Refused {
                    peer: *peer,
                    reason: *reason,
                }));
                self.return_home(EndReason::Released, None, false, false, now, out);
            }
            LinkEvent::Control {
                peer,
                msg: ControlMessage::EndControl { session, reason },
            } if self
                .session()
                .is_some_and(|s| s.peer == *peer && s.id == *session) =>
            {
                out.push(Output::Notice(if *reason == EndReason::TargetLocked {
                    Notice::TargetLocked(*peer)
                } else {
                    Notice::ControlEnded(*peer)
                }));
                self.return_home(*reason, None, false, false, now, out);
            }
            LinkEvent::Input {
                peer,
                msg: InputMessage::Ack { session, seq },
            } => {
                if let Phase::Controlling(c) = &mut self.phase
                    && c.session.peer == *peer
                    && c.session.id == *session
                {
                    c.session.lease.acked(*seq, now);
                }
            }
            LinkEvent::Input {
                peer,
                msg: InputMessage::Status { session, status },
            } if self
                .session()
                .is_some_and(|s| s.peer == *peer && s.id == *session) =>
            {
                match status {
                    TargetStatus::LocalOverride => {
                        out.push(Output::Notice(Notice::LocalOverride(*peer)))
                    }
                    TargetStatus::Refused(reason) => {
                        out.push(Output::Notice(Notice::Refused {
                            peer: *peer,
                            reason: *reason,
                        }));
                        self.return_home(EndReason::Released, None, false, true, now, out);
                    }
                    TargetStatus::Resumed => {}
                }
            }
            _ => {}
        }
    }

    fn end_session(
        &mut self,
        session: &mut Session,
        reason: EndReason,
        send_end: bool,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let held = self.router.release_all(session.peer);
        if self.peers.contains(&session.peer) {
            for item in held {
                session.transition(item, false, now, out);
            }
            if send_end {
                out.push(Output::SendControl {
                    peer: session.peer,
                    msg: ControlMessage::EndControl {
                        session: session.id,
                        reason,
                    },
                });
            }
        }
    }

    fn return_home(
        &mut self,
        reason: EndReason,
        warp_to: Option<(DisplayId, PointDevice)>,
        ended: bool,
        send_end: bool,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        self.push = None;
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        let (capture, session) = match phase {
            Phase::Controlling(c) => (Some(c.capture), Some(c.session)),
            Phase::Crossing(c) => (c.capture, c.session),
            other => {
                self.phase = other;
                return;
            }
        };
        if capture.is_some() && !ended {
            out.push(Output::EndCapture { warp_to });
        }
        if let Some(mut session) = session {
            self.end_session(&mut session, reason, send_end, now, out);
        }
        if let Some(capture) = capture.filter(|_| !ended) {
            self.phase = Phase::Returning {
                capture: capture.id,
                deadline: now.saturating_add(END_TIMEOUT),
            };
        } else {
            self.finish_return(out);
        }
    }

    fn finish_return(&mut self, out: &mut Vec<Output>) {
        self.phase = Phase::Idle;
        self.chord_keys.clear();
        self.capture_buttons.clear();
        self.push = None;
        out.push(Output::HideOverlay(HUD));
    }

    fn disarm(&mut self, out: &mut Vec<Output>) {
        self.armed = false;
        self.push = None;
        if let Some(hold) = &mut self.hotkey {
            hold.rearm = false;
        }
        self.update_portals(out);
    }

    fn arm(&mut self, out: &mut Vec<Output>) {
        self.armed = true;
        self.update_portals(out);
    }

    fn release(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        // Explicit release disarms crossing (04 §6); an ordinary pointer crossing home does not.
        self.return_home(EndReason::Released, None, false, true, now, out);
        self.disarm(out);
    }

    fn panic(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        if let Some(hold) = &mut self.hotkey {
            hold.fired = true;
        }
        self.return_home(EndReason::Panic, None, false, true, now, out);
        self.disarm(out);
        out.push(Output::Notice(Notice::Panic));
    }

    fn hotkey_event(&mut self, event: HotkeyEvent, now: MonoTime, out: &mut Vec<Output>) {
        match event {
            HotkeyEvent::Pressed { at } if self.hotkey.is_none() => {
                let rearm = !self.armed && !self.chord_press_outstanding;
                self.hotkey = Some(HotkeyHold {
                    since: at,
                    fired: false,
                    rearm,
                });
                if self.armed {
                    self.release(now, out);
                }
            }
            HotkeyEvent::Released { .. } => {
                let rearm =
                    self.hotkey.take().is_some_and(|h| h.rearm) && !self.chord_press_outstanding;
                self.chord_press_outstanding = false;
                if rearm && !self.armed && matches!(self.phase, Phase::Idle) {
                    out.push(Output::EngineGate(true));
                    self.arm(out);
                }
            }
            _ => {}
        }
    }

    fn tick(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        if self
            .hotkey
            .as_ref()
            .is_some_and(|h| !h.fired && now >= h.since.saturating_add(self.config.panic_hold))
        {
            // Engine handles Command(Panic)'s gate itself. A timed hotkey panic needs this output.
            out.push(Output::EngineGate(false));
            self.panic(now, out);
        }
        match &mut self.phase {
            Phase::Idle => {
                if let Some(push) = self
                    .push
                    .filter(|p| now >= p.since.saturating_add(self.config.push_to_cross))
                {
                    self.begin_crossing(push, now, out);
                }
            }
            Phase::Crossing(c) => match c.wait {
                Wait::Hud(at) if now >= at => {
                    self.return_home(EndReason::Released, None, false, true, now, out)
                }
                Wait::Handshake(at) if now >= at => {
                    out.push(Output::Notice(Notice::LostConnection(c.entry.0.node)));
                    self.return_home(EndReason::LinkLost, None, false, true, now, out);
                }
                Wait::Capture(at) if now >= at => {
                    self.return_home(EndReason::Released, None, false, true, now, out);
                }
                _ => {}
            },
            Phase::Controlling(c) => {
                if c.session
                    .lease
                    .lost(now, self.rtts.get(&c.session.peer).copied())
                {
                    out.push(Output::Notice(Notice::LostConnection(c.session.peer)));
                    self.return_home(EndReason::LinkLost, None, false, true, now, out);
                    return;
                }
                if !self.ensure_sequence_room(now, out) {
                    return;
                }
                let Phase::Controlling(c) = &mut self.phase else {
                    return;
                };
                let held = self.router.held_on(c.session.peer);
                if now >= c.session.lease.next_heartbeat(!held.is_empty()) {
                    let mut held_keys = Vec::new();
                    let mut held_buttons = Vec::new();
                    for item in held {
                        match item {
                            Held::Key(k) => held_keys.push(k),
                            Held::Button(b) => held_buttons.push(b),
                        }
                    }
                    let (session, seq) = c.session.next_input(now);
                    c.session.lease.heartbeat_sent(now);
                    out.push(Output::SendInput {
                        peer: c.session.peer,
                        msg: InputMessage::State {
                            session,
                            seq,
                            held_keys,
                            held_buttons,
                        },
                    });
                }
            }
            Phase::Returning { deadline, .. } if now >= *deadline => {
                out.push(Output::EndCapture { warp_to: None });
                self.finish_return(out);
            }
            _ => {}
        }
    }
}
