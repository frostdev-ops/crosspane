//! Deterministic, sans-I/O shared audio admission. No sample data enters this module.
//!
//! # Two kinds of speaker stream (AUD-PROJ, "sound follows the window")
//!
//! Every session has an `Origin`:
//!
//! - `Origin::Machine` is whole-machine audio (a virtual speaker, or a microphone). The node
//!   that plays or records it admits it only under its local `speaker` / `mic` grant.
//! - `Origin::Projection` is the sound of the windows one node projects to another. The
//!   destination admits it because it accepted the window (E2 granted `present` and shows the
//!   proxy), never because of the `speaker` grant. The source sends it under the rules below.
//!
//! The two have separate wire messages (`AudioOpen` / `ProjectionAudioOpen`), stream IDs, "one
//! per (peer, kind)" slots and playback endpoints. What E2 knows reaches this module as an
//! `AudioView`, computed after E2 handled the very input that is being handled here and passed
//! to every `Audio::handle` call, so a projection ending, a `present` revoke, a lock or the
//! reconnect grace running out ends projection audio in the same call.
//!
//! Invariants, each enforced in one place (the labels appear at those places):
//!
//! - **D1** (destination admits): `Audio::projection_shown` is the only thing that lets a
//!   `ProjectionAudioOpen` in; the `speaker` grant is never consulted for it.
//! - **D2** (whole-machine unchanged): `Audio::may_open` keeps `AudioOpen` under `has_grant`.
//! - **D3** (destination ends): `Audio::reconcile` ends an admitted projection stream in the
//!   `handle` where the peer leaves the view or loses `audio/1`; `Audio::permitted` is the
//!   origin-aware `admitted`.
//! - **D4**: projection streams are ordinary incoming speaker sessions (indicators, notices,
//!   playback) apart from their origin.
//! - **S1** (source sets): `Audio::wanted_sources` and `Audio::reconcile` emit the pids.
//! - **S2** (legacy peer): `VirtualActive` keeps the edge-triggered `Audio::outgoing`.
//! - **S3** (audio/1 peer): `Audio::reconcile` opens and ends the projection session level
//!   triggered; it is exempt from the "peer's `Grants` lack speaker" rule.
//! - **S4**: `ProjectionAudioOpen` is only sent to a peer in `projection_peers`.
//! - **S5**: microphones never take a projection path.
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crosspane_platform::{AudioDeviceError, AudioEvent, SessionEvent};
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{Capability, ControlMessage, Refusal};
use crosspane_types::audio::{AudioKind, AudioStreamId};
use crosspane_types::id::NodeId;
use crosspane_types::time::MonoTime;

use crate::e2::AudioView;
use crate::io::{AudioEndpoint, AudioKey};
use crate::{Command, Failure, Input, Notice, Output};

/// How long a peer that refused, closed or failed this node's projection stream is left alone
/// before the level-triggered rule asks again (AUD-PROJ S3). A refusal usually names something
/// that fixes itself without any event this node sees: the peer's own audio gate reopening after
/// a panic, an old stream it had not yet ended (the replacement race), or a window it has not
/// shown yet. Without a retry the sound would stay silent until the pid set happened to change.
/// Every failed retry sets the backoff again, so a peer that keeps refusing is asked once per
/// period, never on every input.
const PROJECTION_RETRY: Duration = Duration::from_secs(5);

/// Per-peer admission state. It outlives links: `next` and `remote_high` are the identity
/// counters that keep a stream ID from being reused for the lifetime of this engine (WP-3.0b),
/// so a closed link only clears `up` and `negotiated`.
#[derive(Debug, Default)]
struct Peer {
    /// `PeerUp` arrived and no `Closed` followed. Only an up peer can negotiate audio.
    up: bool,
    negotiated: bool,
    /// The next stream ID this node allocates (odd for the smaller NodeId, even for the larger).
    next: u32,
    /// The highest valid ID the peer has used to open a stream to us.
    remote_high: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Handshake,
    Indicator,
    Opening,
    Active,
}

/// Why a stream exists: who may open it and what ends it (AUD-PROJ). Microphones are always
/// `Machine`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// Whole-machine audio: `AudioOpen`, admitted under the local grant of its kind.
    Machine,
    /// The sound of the sender's windows projected to the receiver: `ProjectionAudioOpen`,
    /// admitted under a live projection and the `audio/1` feature, never under a grant.
    Projection,
}

#[derive(Clone, Copy, Debug)]
struct Session {
    key: AudioKey,
    kind: AudioKind,
    incoming: bool,
    origin: Origin,
    phase: Phase,
    deadline: MonoTime,
}

#[derive(Debug)]
pub(crate) struct Audio {
    node: NodeId,
    peers: BTreeMap<NodeId, Peer>,
    sessions: BTreeMap<(NodeId, AudioStreamId), Session>,
    activity: BTreeSet<(NodeId, AudioKind)>,
    grants: BTreeMap<NodeId, BTreeSet<Capability>>,
    generation: u64,
    session_permits: bool,
    asleep: bool,
    engine_permits: bool,
    /// Whether this node can host microphones at all. False in production speaker-v0.
    mic_support: bool,
    /// AUD-PROJ destination: the peers with a projection shown here (`AudioView::destinations`),
    /// copied at the start of every `handle`. Every `permitted` check in that call reads it.
    shown: BTreeSet<NodeId>,
    /// AUD-PROJ source: this node can capture the sound of projected windows
    /// (`Input::AudioProjectionCapture`). False (the default) switches the whole source side off:
    /// no pid sets, no projection streams, `VirtualActive` behaves exactly as before.
    projection_capture: bool,
    /// Peers whose `Hello` carries `audio/1` on the current link (`Input::ProjectionAudioPeer`).
    projection_peers: BTreeSet<NodeId>,
    /// Peers whose own `Grants` message grants this node `AudioSpeaker` (the legacy consent to
    /// receive projected sound). Forgotten when the link closes or is replaced.
    peer_speaker: BTreeSet<NodeId>,
    /// The pid set last emitted per peer (`Output::AudioProjectionSources`). Only non-empty sets
    /// are kept, so an absent entry means "nothing in force". Forgotten with `RemoveAudioPeer`.
    sources_sent: BTreeMap<NodeId, BTreeSet<u32>>,
    /// Peers that refused or closed (or failed) this node's projection stream, with the time a
    /// retry becomes allowed (`PROJECTION_RETRY` after the refusal). They are asked again at
    /// that time, or earlier when something changes: their pid set, the capture going inactive
    /// then active, or the link. This is what keeps the level-triggered open from becoming a
    /// storm, and what keeps a refused peer from staying silent for good.
    refused: BTreeMap<NodeId, MonoTime>,
    /// The `now` of the `handle` call being processed, for the places that start a backoff
    /// (`refuse_until`) and have no clock of their own.
    now: MonoTime,
}

impl Audio {
    pub(crate) fn new(node: NodeId) -> Self {
        Self {
            node,
            peers: BTreeMap::new(),
            sessions: BTreeMap::new(),
            activity: BTreeSet::new(),
            grants: BTreeMap::new(),
            generation: 0,
            session_permits: false,
            asleep: false,
            engine_permits: true,
            mic_support: false,
            shown: BTreeSet::new(),
            projection_capture: false,
            projection_peers: BTreeSet::new(),
            peer_speaker: BTreeSet::new(),
            sources_sent: BTreeMap::new(),
            refused: BTreeMap::new(),
            now: MonoTime::ZERO,
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<MonoTime> {
        // S3 retry: only for a peer where the retry could really fire (the same test as the
        // rule itself, minus the time), so the agent's `Tick` is never asked for in vain.
        let retry = self
            .refused
            .iter()
            .filter(|(peer, _)| self.can_open_projection(**peer))
            .map(|(_, at)| *at);
        self.sessions
            .values()
            .filter(|s| s.phase != Phase::Active)
            .map(|s| s.deadline)
            .chain(retry)
            .min()
    }

    // Also receives EngineGate outputs from E1's timed hotkey panic/rearm, without altering E1.
    pub(crate) fn engine_gate(&mut self, permits: bool, out: &mut Vec<Output>) {
        self.engine_permits = permits;
        if !permits {
            self.end_where(|_| true, Some(Refusal::Locked), out);
        }
    }

    /// Handle one input. `view` is what E2 knows after it handled this same input (the engine
    /// computes it once E2 and the drag commit are done), so everything that follows from a
    /// projection starting or ending happens in this call, not on a later one.
    pub(crate) fn handle(
        &mut self,
        input: &Input,
        now: MonoTime,
        view: &AudioView,
        out: &mut Vec<Output>,
    ) {
        self.now = now;
        self.shown.clone_from(&view.destinations);
        // Expire before processing callbacks, even if the agent has not delivered Tick yet.
        self.end_where(
            |s| s.phase != Phase::Active && now >= s.deadline,
            Some(Refusal::InjectorFailed),
            out,
        );
        self.dispatch(input, now, out);
        self.reconcile(now, view, out);
    }

    fn dispatch(&mut self, input: &Input, now: MonoTime, out: &mut Vec<Output>) {
        match input {
            Input::PeerUp { peer } => {
                if *peer != self.node {
                    // An existing entry keeps its identity counters: a new link never resets them.
                    self.peers
                        .entry(*peer)
                        .or_insert_with(|| Peer {
                            next: if self.node < *peer { 1 } else { 2 },
                            ..Peer::default()
                        })
                        .up = true;
                }
            }
            Input::AudioPeer {
                peer,
                name,
                available,
            } => {
                if let Some(p) = self.peers.get_mut(peer).filter(|p| p.up) {
                    let changed = p.negotiated != *available;
                    p.negotiated = *available;
                    if changed {
                        if *available {
                            out.push(Output::AddAudioPeer {
                                peer: *peer,
                                name: name.clone(),
                            });
                        } else {
                            self.end_where(|s| s.key.peer == *peer, Some(Refusal::Permission), out);
                            self.activity.retain(|(p, _)| p != peer);
                            out.push(Output::RemoveAudioPeer { peer: *peer });
                            // The device is gone with its pid set, and a re-add negotiates
                            // `audio/1` afresh (S1: a re-add re-sends the set).
                            self.forget_projection_link(*peer);
                        }
                    }
                }
            }
            Input::AudioProjectionCapture { available } => {
                // Switching it off needs no work here: `reconcile` ends the streams and
                // empties the pid sets.
                self.projection_capture = *available;
            }
            Input::ProjectionAudioPeer { peer, available } => {
                let up = self.peers.get(peer).is_some_and(|p| p.up);
                let changed = if *available {
                    // Only a peer with a live link can negotiate it (like `AudioPeer`).
                    up && self.projection_peers.insert(*peer)
                } else {
                    self.projection_peers.remove(peer)
                };
                if changed {
                    // A fresh negotiation: whatever the peer refused under the old one is moot.
                    // A lost feature ends the streams in `reconcile` (S3, D3).
                    self.refused.remove(peer);
                }
            }
            Input::Link(LinkEvent::Closed { peer, .. }) => {
                self.end_where(|s| s.key.peer == *peer, None, out);
                self.activity.retain(|(p, _)| p != peer);
                // A true disconnect removes availability and devices but keeps the peer's
                // identity counters, so a reconnect cannot reuse a stream ID.
                if let Some(p) = self.peers.get_mut(peer) {
                    p.up = false;
                    if std::mem::take(&mut p.negotiated) {
                        out.push(Output::RemoveAudioPeer { peer: *peer });
                    }
                }
                self.forget_projection_link(*peer);
                self.peer_speaker.remove(peer);
            }
            Input::AudioConnectionReplaced { peer } => {
                // A silent supersession keeps the logical link, so availability, the identity
                // counters, admission generations and the retained device demand all stay. Only
                // the sessions bound to the old connection end; demand restarts only after the
                // app goes inactive and active again.
                self.end_where(|s| s.key.peer == *peer, None, out);
                // The new connection sends its own `Grants`, and a peer that refused on the old
                // one may accept on the new one. An `audio/1` projection stream (S3) does not
                // wait for an activity edge: `reconcile` reopens it in this very call.
                self.peer_speaker.remove(peer);
                self.refused.remove(peer);
            }
            Input::AudioStreamFailed { key } => {
                // Only the complete currently admitted key: a stale failure of an older
                // generation (or of an already ended session) cannot touch a newer admission.
                let id = (key.peer, key.stream);
                if self.sessions.get(&id).is_some_and(|s| s.key == *key) {
                    self.end(id, Some(Refusal::InjectorFailed), out);
                }
            }
            Input::AudioMicrophoneSupport { available } => {
                self.mic_support = *available;
                if !*available {
                    // Demand latches stay: nothing restarts until the app goes inactive, active.
                    self.end_where(
                        |s| s.kind == AudioKind::Microphone,
                        Some(Refusal::InjectorFailed),
                        out,
                    );
                }
            }
            Input::Session(event) => {
                match event {
                    SessionEvent::State(state) => self.session_permits = state.permits_io(),
                    SessionEvent::WillSleep => {
                        self.asleep = true;
                        self.session_permits = false;
                    }
                    SessionEvent::Woke => {
                        self.asleep = false;
                        self.session_permits = false;
                    }
                    _ => self.session_permits = false,
                }
                if !self.gate() {
                    self.end_where(|_| true, Some(Refusal::Locked), out);
                }
            }
            Input::Command(Command::Panic) => self.engine_gate(false, out),
            Input::Command(Command::Rearm) => self.engine_gate(true, out),
            Input::Grants(grants) => {
                let old_grants = std::mem::replace(&mut self.grants, grants.clone());
                let remove: Vec<_> = self
                    .sessions
                    .values()
                    .filter(|s| {
                        let removed = old_grants
                            .get(&s.key.peer)
                            .is_some_and(|g| g.contains(&capability(s.kind)));
                        // D3: a projection stream is not a use of the `speaker` grant, so
                        // revoking the grant never touches it (E2's `present` decides).
                        s.origin == Origin::Machine
                            && !self.has_grant(s.key.peer, s.kind)
                            && (s.incoming || removed)
                    })
                    .map(|s| (s.key.peer, s.key.stream))
                    .collect();
                for id in remove {
                    self.end(id, Some(Refusal::Permission), out);
                }
            }
            Input::Audio(AudioEvent::VirtualActive { peer, kind, active }) => {
                if !*active {
                    self.activity.remove(&(*peer, *kind));
                    self.end_where(
                        |s| !s.incoming && s.key.peer == *peer && s.kind == *kind,
                        None,
                        out,
                    );
                } else if self.connected(*peer) && self.activity.insert((*peer, *kind)) {
                    if self.follows_projection(*peer, *kind) {
                        // S3: the capture of an `audio/1` peer's projected windows became
                        // active. No legacy `AudioOpen`; `reconcile` opens the projection
                        // stream (level triggered) once the pid set and the gate allow it.
                        // An inactive -> active edge lets a peer that refused be asked again.
                        self.refused.remove(peer);
                    } else {
                        // S2 (and every microphone, S5): edge triggered, as before.
                        self.outgoing(*peer, *kind, now, out);
                    }
                }
            }
            Input::Audio(AudioEvent::DeviceError { peer, kind, error }) => {
                let reason = match error {
                    AudioDeviceError::Locked => Refusal::Locked,
                    AudioDeviceError::PermissionDenied => Refusal::Permission,
                    _ => Refusal::InjectorFailed,
                };
                self.end_where(
                    |s| {
                        s.kind == *kind
                            && match peer {
                                None => s.incoming,
                                Some(p) => !s.incoming && s.key.peer == *p,
                            }
                    },
                    Some(reason),
                    out,
                );
            }
            Input::AudioIndicatorShown { key, visible } => self.indicator(*key, *visible, out),
            Input::AudioDeviceOpened { key, kind, result } => {
                self.device(*key, *kind, *result, out)
            }
            Input::Link(LinkEvent::Control { peer, msg }) => match msg {
                ControlMessage::AudioOpen {
                    stream,
                    kind,
                    channels,
                } => self.incoming(*peer, *stream, *kind, *channels, now, out),
                ControlMessage::ProjectionAudioOpen { stream } => {
                    // D1: the same admission as `AudioOpen`, under `Origin::Projection`.
                    let kind = AudioKind::Speaker;
                    let channels = kind.format().channels as u8;
                    self.admit(*peer, *stream, kind, channels, Origin::Projection, now, out);
                }
                ControlMessage::AudioOpened { stream } => self.opened(*peer, *stream, out),
                ControlMessage::AudioClose { stream } => {
                    self.mark_refused_if_projection(*peer, *stream);
                    self.end((*peer, *stream), None, out);
                }
                ControlMessage::AudioRefused { stream, reason } => {
                    if self
                        .sessions
                        .get(&(*peer, *stream))
                        .is_some_and(|s| !s.incoming)
                    {
                        self.mark_refused_if_projection(*peer, *stream);
                        self.end((*peer, *stream), Some(*reason), out);
                    }
                }
                ControlMessage::Grants(grants) => {
                    // S1: remember the peer's consent to receive projected sound (legacy peers).
                    if self.peers.get(peer).is_some_and(|p| p.up) {
                        if grants.contains(&Capability::AudioSpeaker) {
                            self.peer_speaker.insert(*peer);
                        } else {
                            self.peer_speaker.remove(peer);
                        }
                    }
                    self.end_where(
                        |s| {
                            // S3: a projection stream is exempt; it never needed the grant.
                            s.origin == Origin::Machine
                                && s.key.peer == *peer
                                && !s.incoming
                                && !grants.contains(&capability(s.kind))
                        },
                        Some(Refusal::Permission),
                        out,
                    );
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// The end-of-call pass of AUD-PROJ. It is level triggered: it looks at the facts as they are
    /// after this input (E2's `view`, the links, the gate, the capture activity) and makes the
    /// streams and pid sets agree, whichever input changed them. That is what ends projection
    /// audio in the same call as the projection, and what reopens it after a replacement without
    /// a new activity edge. It does nothing on a node that never saw a projection peer, so the
    /// whole-machine behaviour is untouched.
    fn reconcile(&mut self, now: MonoTime, view: &AudioView, out: &mut Vec<Output>) {
        // D3: an admitted projection stream ends, with a plain `AudioClose` and no notice, in the
        // call where its peer leaves `view.destinations` (a return, close, end, revoked
        // `present`, lock, panic, or the reconnect grace running out) or loses `audio/1`. A
        // revoked local `speaker` grant is not among the reasons.
        let stale: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                s.incoming && s.origin == Origin::Projection && !self.projection_shown(s.key.peer)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.end(id, None, out);
        }

        // S1: what each peer should capture now.
        let wanted = self.wanted_sources(view);

        // S3: an outgoing projection stream lives only while its peer's set is non-empty and the
        // peer still has `audio/1`; it ends, with a plain `AudioClose`, in the call where that
        // stops being true. (Capture going inactive, the peer refusing or closing, the gate,
        // panic and a dropped link end it where they happen.)
        let over: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                !s.incoming
                    && s.origin == Origin::Projection
                    && !(self.can_send_projection(s.key.peer) && wanted.contains_key(&s.key.peer))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in over {
            self.end(id, None, out);
        }

        // S1: tell the host only when a peer's set changed (and again after a re-add, because
        // `RemoveAudioPeer` forgot it). A changed set lets a peer that refused be asked again.
        for (peer, pids) in &wanted {
            if self.sources_sent.get(peer) != Some(pids) {
                self.sources_sent.insert(*peer, pids.clone());
                self.refused.remove(peer);
                out.push(Output::AudioProjectionSources {
                    peer: *peer,
                    pids: pids.iter().copied().collect(),
                });
            }
        }
        let emptied: Vec<_> = self
            .sources_sent
            .keys()
            .filter(|peer| !wanted.contains_key(*peer))
            .copied()
            .collect();
        for peer in emptied {
            self.sources_sent.remove(&peer);
            self.refused.remove(&peer);
            out.push(Output::AudioProjectionSources {
                peer,
                pids: Vec::new(),
            });
        }

        // S3: open the projection stream of every `audio/1` peer whose windows are being
        // captured. Level triggered: capture active, set non-empty (it is in `wanted`), gate
        // open, nothing carrying that capture to the peer yet (a legacy stream that predates the
        // peer's `audio/1` already does), and the peer is not in a refusal backoff (a refusal
        // waits for a change in the pid set, the capture or the link, or `PROJECTION_RETRY`).
        let peers: Vec<_> = wanted.keys().copied().collect();
        for peer in peers {
            let backed_off = self.refused.get(&peer).is_some_and(|until| now < *until);
            if !self.can_open_projection(peer) || backed_off {
                continue;
            }
            if self.open_outgoing(peer, AudioKind::Speaker, Origin::Projection, now, out) {
                // The retry (if this was one) is over; a new refusal starts a new backoff.
                self.refused.remove(&peer);
            } else {
                // IDs or generations ran out: do not retry on every input.
                self.refuse_until(peer);
            }
        }
    }
    /// S3: the level-triggered rule's conditions for opening a projection stream to `peer`, apart
    /// from the refusal backoff: this node captures projections, the gate is open, the pid set it
    /// sent the peer is non-empty, the peer has `audio/1`, the capture is active, and nothing
    /// (a legacy stream that predates `audio/1` included) already carries that capture to it.
    /// `next_deadline` asks the same question, so a retry is only waited for when it can fire.
    fn can_open_projection(&self, peer: NodeId) -> bool {
        self.projection_capture
            && self.gate()
            && self.sources_sent.contains_key(&peer)
            && self.can_send_projection(peer) // S4
            && self.activity.contains(&(peer, AudioKind::Speaker))
            && !self
                .sessions
                .values()
                .any(|s| !s.incoming && s.key.peer == peer && s.kind == AudioKind::Speaker)
    }
    /// Leave `peer` alone for `PROJECTION_RETRY`: it refused, closed or failed the stream.
    fn refuse_until(&mut self, peer: NodeId) {
        let until = self.now.saturating_add(PROJECTION_RETRY);
        self.refused.insert(peer, until);
    }
    /// S1: the pids each peer's capture should cover. A peer gets `view.sources[peer]` when it is
    /// connected and either negotiated `audio/1` or (a legacy peer) granted this node
    /// `AudioSpeaker`; otherwise nothing, so a peer that did not accept the sound gets none of
    /// it. Empty sets are left out. Empty everywhere unless this node can capture projections,
    /// and empty while the audio gate is shut (a hotkey panic, lock or sleep): the host stops
    /// its capture rather than keep recording the window's process with nowhere to send it, and
    /// the set is sent again when the gate reopens.
    fn wanted_sources(&self, view: &AudioView) -> BTreeMap<NodeId, BTreeSet<u32>> {
        if !self.projection_capture || !self.gate() {
            return BTreeMap::new();
        }
        view.sources
            .iter()
            .filter(|(peer, pids)| {
                !pids.is_empty()
                    && self.connected(**peer)
                    && (self.projection_peers.contains(*peer) || self.peer_speaker.contains(*peer))
            })
            .map(|(peer, pids)| (*peer, pids.clone()))
            .collect()
    }

    fn gate(&self) -> bool {
        self.session_permits && !self.asleep && self.engine_permits
    }
    fn connected(&self, peer: NodeId) -> bool {
        self.peers.get(&peer).is_some_and(|p| p.up && p.negotiated)
    }
    fn has_grant(&self, peer: NodeId, kind: AudioKind) -> bool {
        self.grants
            .get(&peer)
            .is_some_and(|g| g.contains(&capability(kind)))
    }
    /// Whether this session may still run: the gate is open, the peer is connected, and the
    /// permission of its origin holds. D3: the origin decides what that permission is.
    fn admitted(&self, s: Session) -> bool {
        self.gate()
            && self.connected(s.key.peer)
            && (s.kind != AudioKind::Microphone || self.mic_support)
            && self.permitted(&s)
    }
    /// The origin-specific permission behind a session (`admitted` adds the gate and link).
    fn permitted(&self, s: &Session) -> bool {
        match (s.incoming, s.origin) {
            (true, Origin::Machine) => self.has_grant(s.key.peer, s.kind),
            (true, Origin::Projection) => self.projection_shown(s.key.peer),
            // The machine's own device capture needs nothing beyond the gate and the link.
            (false, Origin::Machine) => true,
            (false, Origin::Projection) => self.can_send_projection(s.key.peer),
        }
    }
    /// What lets `peer` open a stream of `origin` to this node (D1, D2).
    fn may_open(&self, peer: NodeId, kind: AudioKind, origin: Origin) -> bool {
        match origin {
            // D2: whole-machine audio needs the local grant, even while `peer` projects here.
            Origin::Machine => self.has_grant(peer, kind),
            // D1: a projection stream needs a projection shown here and `audio/1`. The local
            // `speaker` grant is deliberately not consulted.
            Origin::Projection => self.projection_shown(peer),
        }
    }
    /// D1/D3: `peer` has `audio/1` and at least one projection shown on this node right now
    /// (E2 granted `present`, the proxy is open and the link is not suspended).
    fn projection_shown(&self, peer: NodeId) -> bool {
        self.projection_peers.contains(&peer) && self.shown.contains(&peer)
    }
    /// S4: the only peers a `ProjectionAudioOpen` may be sent to.
    fn can_send_projection(&self, peer: NodeId) -> bool {
        self.projection_capture && self.projection_peers.contains(&peer)
    }
    /// Whether the speaker capture of this node is the sound of projected windows, and `peer`
    /// takes it as a projection stream (S3) rather than as the legacy whole-machine stream (S2).
    fn follows_projection(&self, peer: NodeId, kind: AudioKind) -> bool {
        kind == AudioKind::Speaker && self.can_send_projection(peer)
    }
    /// Forget what was learned about `peer` on a link or audio negotiation that is gone.
    fn forget_projection_link(&mut self, peer: NodeId) {
        self.projection_peers.remove(&peer);
        self.refused.remove(&peer);
        self.sources_sent.remove(&peer);
    }
    /// An `AudioClose` or `AudioRefused` from `peer` for our outgoing projection stream: do not
    /// ask again until the pid set, the capture activity or the link changes, or the backoff
    /// (`PROJECTION_RETRY`) runs out.
    fn mark_refused_if_projection(&mut self, peer: NodeId, stream: AudioStreamId) {
        if self
            .sessions
            .get(&(peer, stream))
            .is_some_and(|s| !s.incoming && s.origin == Origin::Projection)
        {
            self.refuse_until(peer);
        }
    }
    fn key(&mut self, peer: NodeId, stream: AudioStreamId) -> Option<AudioKey> {
        self.generation = self.generation.checked_add(1)?;
        Some(AudioKey {
            peer,
            stream,
            generation: self.generation,
        })
    }
    fn notice(peer: NodeId, kind: AudioKind, reason: Refusal, out: &mut Vec<Output>) {
        out.push(Output::Notice(Notice::AudioRefused { peer, kind, reason }));
    }
    fn refuse(
        peer: NodeId,
        stream: AudioStreamId,
        kind: AudioKind,
        reason: Refusal,
        out: &mut Vec<Output>,
    ) {
        out.push(Output::SendControl {
            peer,
            msg: ControlMessage::AudioRefused { stream, reason },
        });
        Self::notice(peer, kind, reason, out);
    }
    /// Open a whole-machine stream to `peer` (S2, microphones): edge triggered by the caller.
    fn outgoing(&mut self, peer: NodeId, kind: AudioKind, now: MonoTime, out: &mut Vec<Output>) {
        self.open_outgoing(peer, kind, Origin::Machine, now, out);
    }
    /// Allocate a stream ID and send the open message of `origin`. Returns whether a stream was
    /// opened; every failure has pushed its refusal notice already.
    fn open_outgoing(
        &mut self,
        peer: NodeId,
        kind: AudioKind,
        origin: Origin,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> bool {
        // S4/S5: the only place a `ProjectionAudioOpen` is built, and it is only for a speaker
        // stream to a peer that negotiated `audio/1`, from a node that captures projections.
        if origin == Origin::Projection
            && (kind != AudioKind::Speaker || !self.can_send_projection(peer))
        {
            return false;
        }
        if !self.connected(peer) {
            Self::notice(peer, kind, Refusal::Permission, out);
            return false;
        }
        if kind == AudioKind::Microphone && !self.mic_support {
            // Unsupported: no AudioOpen, no stream ID consumed, only the refusal notice.
            Self::notice(peer, kind, Refusal::InjectorFailed, out);
            return false;
        }
        if !self.gate() {
            Self::notice(peer, kind, Refusal::Locked, out);
            return false;
        }
        let Some(p) = self.peers.get_mut(&peer) else {
            return false;
        };
        if p.next > u16::MAX as u32 {
            Self::notice(peer, kind, Refusal::Busy, out);
            return false;
        }
        let stream = AudioStreamId(p.next as u16);
        p.next += 2;
        let Some(key) = self.key(peer, stream) else {
            Self::notice(peer, kind, Refusal::Busy, out);
            return false;
        };
        self.sessions.insert(
            (peer, stream),
            Session {
                key,
                kind,
                incoming: false,
                origin,
                phase: Phase::Handshake,
                deadline: now.saturating_add(Duration::from_secs(2)),
            },
        );
        out.push(Output::SendControl {
            peer,
            msg: match origin {
                Origin::Machine => ControlMessage::AudioOpen {
                    stream,
                    kind,
                    channels: kind.format().channels as u8,
                },
                // S4: guarded above; only a peer in `projection_peers` is ever sent this.
                Origin::Projection => ControlMessage::ProjectionAudioOpen { stream },
            },
        });
        true
    }
    /// Admit (or refuse) a whole-machine `AudioOpen` (D2).
    fn incoming(
        &mut self,
        peer: NodeId,
        stream: AudioStreamId,
        kind: AudioKind,
        channels: u8,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        self.admit(peer, stream, kind, channels, Origin::Machine, now, out);
    }
    /// Admit (or refuse) an incoming open of either origin. The stream ID is consumed, and the
    /// refusal reasons and their order are the same for both: Permission, then the microphone
    /// support, Locked, Busy. Only `may_open` differs (D1/D2), and the Busy slot is per origin.
    #[allow(clippy::too_many_arguments)]
    fn admit(
        &mut self,
        peer: NodeId,
        stream: AudioStreamId,
        kind: AudioKind,
        channels: u8,
        origin: Origin,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let valid_id = stream.0 != 0 && (stream.0 % 2 == 1) == (peer < self.node);
        let fresh = self
            .peers
            .get(&peer)
            .is_some_and(|p| p.up && stream.0 > p.remote_high);
        // Consume fresh remote IDs even when admission fails. Ordered control stream bounds replay state.
        if valid_id
            && fresh
            && let Some(p) = self.peers.get_mut(&peer)
        {
            p.remote_high = stream.0;
        }
        let reason = if !self.connected(peer)
            || !valid_id
            || !fresh
            || channels != kind.format().channels as u8
            || !self.may_open(peer, kind, origin)
        {
            Some(Refusal::Permission)
        } else if kind == AudioKind::Microphone && !self.mic_support {
            // A valid ID was consumed above; no indicator, notice of use or capture follows.
            Some(Refusal::InjectorFailed)
        } else if !self.gate() {
            Some(Refusal::Locked)
        } else if self
            .sessions
            .values()
            // D2: a projection stream and a whole-machine stream from one peer have separate slots.
            .any(|s| s.incoming && s.key.peer == peer && s.kind == kind && s.origin == origin)
        {
            Some(Refusal::Busy)
        } else {
            None
        };
        if let Some(reason) = reason {
            // A repeat cannot revoke admission on the wire while leaving its physical
            // endpoint alive. Only incoming sessions match: remote requests must
            // never tear down an outgoing stream in the local parity namespace.
            if self
                .sessions
                .get(&(peer, stream))
                .is_some_and(|s| s.incoming)
            {
                self.end((peer, stream), None, out);
            }
            Self::refuse(peer, stream, kind, reason, out);
            return;
        }
        let Some(key) = self.key(peer, stream) else {
            Self::refuse(peer, stream, kind, Refusal::Busy, out);
            return;
        };
        let phase = if kind == AudioKind::Microphone {
            Phase::Indicator
        } else {
            Phase::Opening
        };
        self.sessions.insert(
            (peer, stream),
            Session {
                key,
                kind,
                incoming: true,
                origin,
                phase,
                deadline: now.saturating_add(Duration::from_secs(2)),
            },
        );
        // D4: a projection stream is announced, indicated and played like a speaker stream.
        out.push(Output::Notice(match kind {
            AudioKind::Microphone => Notice::MicInUseBy(peer),
            AudioKind::Speaker => Notice::SpeakerInUseBy(peer),
        }));
        self.indicators(out);
        if kind == AudioKind::Speaker {
            out.push(Output::OpenAudioPlayback { key });
        }
    }
    fn indicator(&mut self, key: AudioKey, visible: bool, out: &mut Vec<Output>) {
        let id = (key.peer, key.stream);
        let Some(s) = self
            .sessions
            .get(&id)
            .copied()
            .filter(|s| s.key == key && s.incoming && s.kind == AudioKind::Microphone)
        else {
            return;
        };
        if !visible || !self.admitted(s) {
            self.end(id, Some(Refusal::InjectorFailed), out);
            return;
        }
        if s.phase == Phase::Indicator {
            if let Some(s) = self.sessions.get_mut(&id) {
                s.phase = Phase::Opening;
            }
            out.push(Output::OpenAudioCapture { key });
        }
    }
    fn device(
        &mut self,
        key: AudioKey,
        kind: AudioKind,
        result: Result<(), Failure>,
        out: &mut Vec<Output>,
    ) {
        let id = (key.peer, key.stream);
        let current = self.sessions.get(&id).copied().filter(|s| s.key == key);
        let Some(s) = current else {
            if result.is_ok() {
                close(key, kind, out);
            }
            return;
        };
        if !s.incoming || s.kind != kind || s.phase != Phase::Opening || !self.admitted(s) {
            // Unexpected success owns only the supplied complete key and kind, never a newer session.
            if result.is_ok() {
                out.push(Output::StopAudioStream { key });
                close(key, kind, out);
            }
            // Duplicate success for the active endpoint must fail closed as its handle was closed.
            self.end(id, Some(Refusal::InjectorFailed), out);
            return;
        }
        if let Err(error) = result {
            let reason = match error {
                Failure::Locked | Failure::SecureInput => Refusal::Locked,
                Failure::PermissionDenied => Refusal::Permission,
                _ => Refusal::InjectorFailed,
            };
            self.end(id, Some(reason), out);
            return;
        }
        if let Some(s) = self.sessions.get_mut(&id) {
            s.phase = Phase::Active;
        }
        out.push(Output::StartAudioStream {
            key,
            kind,
            endpoint: match kind {
                AudioKind::Microphone => AudioEndpoint::LocalCapture,
                AudioKind::Speaker => AudioEndpoint::LocalPlayback,
            },
        });
        out.push(Output::SendControl {
            peer: key.peer,
            msg: ControlMessage::AudioOpened { stream: key.stream },
        });
    }
    fn opened(&mut self, peer: NodeId, stream: AudioStreamId, out: &mut Vec<Output>) {
        let id = (peer, stream);
        if self.sessions.get(&id).is_some_and(|s| s.incoming) {
            // A response cannot authorize an incoming request. End its admission before
            // replying, so pending callbacks cannot open or retain a physical device.
            self.end(id, None, out);
            return;
        }
        if let Some(s) = self.sessions.get(&id).copied().filter(|s| !s.incoming) {
            if s.phase == Phase::Active {
                return;
            }
            if s.phase == Phase::Handshake && self.admitted(s) {
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.phase = Phase::Active;
                }
                out.push(Output::StartAudioStream {
                    key: s.key,
                    kind: s.kind,
                    endpoint: match s.kind {
                        AudioKind::Speaker => AudioEndpoint::VirtualSpeaker,
                        AudioKind::Microphone => AudioEndpoint::VirtualMicrophone,
                    },
                });
                return;
            }
            self.end(id, Some(Refusal::InjectorFailed), out);
        }
        out.push(Output::SendControl {
            peer,
            msg: ControlMessage::AudioClose { stream },
        });
    }
    fn end_where(
        &mut self,
        matches: impl Fn(&Session) -> bool,
        reason: Option<Refusal>,
        out: &mut Vec<Output>,
    ) {
        let ids: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, s)| matches(s))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.end(id, reason, out);
        }
    }
    fn end(&mut self, id: (NodeId, AudioStreamId), reason: Option<Refusal>, out: &mut Vec<Output>) {
        let Some(s) = self.sessions.remove(&id) else {
            return;
        };
        // S3 (no storm): an outgoing projection stream that ends for a failure (a refusal, a
        // device error, no answer in time) is not reopened by the level-triggered rule until
        // something changes. Ending it ourselves (`None`) or because the gate closed (`Locked`,
        // reopened when the gate does) leaves the peer askable.
        if !s.incoming
            && s.origin == Origin::Projection
            && reason.is_some_and(|r| r != Refusal::Locked)
        {
            self.refuse_until(s.key.peer);
        }
        out.push(Output::StopAudioStream { key: s.key });
        if s.incoming && s.phase != Phase::Indicator {
            close(s.key, s.kind, out);
        }
        if let Some(reason) = reason {
            if s.incoming && s.phase != Phase::Active {
                Self::refuse(s.key.peer, s.key.stream, s.kind, reason, out);
            } else {
                out.push(Output::SendControl {
                    peer: s.key.peer,
                    msg: ControlMessage::AudioClose {
                        stream: s.key.stream,
                    },
                });
                Self::notice(s.key.peer, s.kind, reason, out);
            }
        } else {
            out.push(Output::SendControl {
                peer: s.key.peer,
                msg: ControlMessage::AudioClose {
                    stream: s.key.stream,
                },
            });
        }
        if s.incoming {
            self.indicators(out);
        }
    }
    fn indicators(&self, out: &mut Vec<Output>) {
        let mut microphones = Vec::new();
        let mut speakers = Vec::new();
        for s in self.sessions.values().filter(|s| s.incoming) {
            match s.kind {
                AudioKind::Microphone => microphones.push(s.key),
                AudioKind::Speaker => speakers.push(s.key),
            }
        }
        microphones.sort_unstable();
        speakers.sort_unstable();
        out.push(Output::AudioIndicators {
            microphones,
            speakers,
        });
    }
}

fn capability(kind: AudioKind) -> Capability {
    match kind {
        AudioKind::Speaker => Capability::AudioSpeaker,
        AudioKind::Microphone => Capability::AudioMic,
    }
}
fn close(key: AudioKey, kind: AudioKind, out: &mut Vec<Output>) {
    out.push(match kind {
        AudioKind::Microphone => Output::CloseAudioCapture { key },
        AudioKind::Speaker => Output::CloseAudioPlayback { key },
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: NodeId = NodeId([1; 32]);
    const PEER: NodeId = NodeId([2; 32]);

    fn ready() -> Audio {
        let mut audio = Audio::new(NODE);
        audio.session_permits = true;
        // These synthetic tests exercise the future microphone paths explicitly.
        audio.mic_support = true;
        audio.peers.insert(
            PEER,
            Peer {
                up: true,
                negotiated: true,
                next: 1,
                remote_high: 0,
            },
        );
        audio.grants.insert(
            PEER,
            BTreeSet::from([Capability::AudioMic, Capability::AudioSpeaker]),
        );
        audio
    }

    #[test]
    fn stream_id_exhaustion_never_wraps_and_failed_opens_consume_ids() {
        for (node, last) in [(NODE, u16::MAX), (NodeId([3; 32]), u16::MAX - 1)] {
            let mut audio = ready();
            audio.node = node;
            audio.peers.get_mut(&PEER).unwrap().next = u32::from(last);
            let mut out = Vec::new();
            audio.outgoing(PEER, AudioKind::Microphone, MonoTime::ZERO, &mut out);
            assert!(out.contains(&Output::SendControl {
                peer: PEER,
                msg: ControlMessage::AudioOpen {
                    stream: AudioStreamId(last),
                    kind: AudioKind::Microphone,
                    channels: 1
                }
            }));
            audio.end(
                (PEER, AudioStreamId(last)),
                Some(Refusal::Permission),
                &mut out,
            );
            out.clear();
            audio.outgoing(PEER, AudioKind::Speaker, MonoTime::ZERO, &mut out);
            assert_eq!(
                out,
                vec![Output::Notice(Notice::AudioRefused {
                    peer: PEER,
                    kind: AudioKind::Speaker,
                    reason: Refusal::Busy
                })]
            );
            assert!(audio.sessions.is_empty());
            assert!(audio.peers[&PEER].next > u32::from(u16::MAX));
        }
    }

    #[test]
    fn admission_generation_exhaustion_never_reuses_even_after_reconnect() {
        let mut audio = ready();
        audio.generation = u64::MAX - 1;
        let mut out = Vec::new();
        audio.incoming(
            PEER,
            AudioStreamId(2),
            AudioKind::Microphone,
            1,
            MonoTime::ZERO,
            &mut out,
        );
        let key = audio.sessions[&(PEER, AudioStreamId(2))].key;
        assert_eq!(key.generation, u64::MAX);
        audio.handle(
            &Input::Link(LinkEvent::Closed {
                peer: PEER,
                error: crosspane_protocol::link::LinkError::Closed,
            }),
            MonoTime::ZERO,
            &AudioView::default(),
            &mut out,
        );
        audio.handle(
            &Input::PeerUp { peer: PEER },
            MonoTime::ZERO,
            &AudioView::default(),
            &mut out,
        );
        audio.handle(
            &Input::AudioPeer {
                peer: PEER,
                name: String::new(),
                available: true,
            },
            MonoTime::ZERO,
            &AudioView::default(),
            &mut out,
        );
        out.clear();
        // The reconnect cannot reuse ID 2 (WP-3.0b); a higher ID reaches generation exhaustion.
        audio.incoming(
            PEER,
            AudioStreamId(4),
            AudioKind::Microphone,
            1,
            MonoTime::ZERO,
            &mut out,
        );
        assert!(audio.sessions.is_empty());
        assert!(out.contains(&Output::SendControl {
            peer: PEER,
            msg: ControlMessage::AudioRefused {
                stream: AudioStreamId(4),
                reason: Refusal::Busy
            }
        }));
        out.clear();
        audio.outgoing(PEER, AudioKind::Microphone, MonoTime::ZERO, &mut out);
        assert!(audio.sessions.is_empty());
        assert!(!out.iter().any(|o| matches!(
            o,
            Output::OpenAudioCapture { .. }
                | Output::SendControl {
                    msg: ControlMessage::AudioOpen { .. },
                    ..
                }
        )));
        assert_eq!(audio.generation, u64::MAX);
    }

    /// A node that captures projections, whose `PEER` negotiated `audio/1` and whose capture of
    /// the pids in `sources` is active.
    fn projecting(sources: &[u32]) -> (Audio, AudioView) {
        let mut audio = ready();
        audio.projection_capture = true;
        audio.projection_peers.insert(PEER);
        audio.activity.insert((PEER, AudioKind::Speaker));
        let view = AudioView {
            sources: BTreeMap::from([(PEER, sources.iter().copied().collect())]),
            destinations: BTreeSet::new(),
        };
        (audio, view)
    }

    #[test]
    fn a_projection_stream_that_cannot_get_an_id_is_asked_for_once_not_on_every_input() {
        let (mut audio, view) = projecting(&[42]);
        audio.peers.get_mut(&PEER).unwrap().next = u32::from(u16::MAX) + 1;
        let mut out = Vec::new();
        audio.handle(&Input::Tick, MonoTime::ZERO, &view, &mut out);
        assert_eq!(
            out,
            vec![
                Output::AudioProjectionSources {
                    peer: PEER,
                    pids: vec![42]
                },
                Output::Notice(Notice::AudioRefused {
                    peer: PEER,
                    kind: AudioKind::Speaker,
                    reason: Refusal::Busy
                })
            ]
        );
        for _ in 0..3 {
            out.clear();
            audio.handle(&Input::Tick, MonoTime::ZERO, &view, &mut out);
            assert!(out.is_empty(), "{out:?}");
        }
        assert!(audio.sessions.is_empty());
    }

    #[test]
    fn a_projection_stream_is_only_ever_opened_for_a_speaker_to_an_audio1_peer() {
        let (mut audio, _) = projecting(&[42]);
        audio.projection_peers.clear();
        let mut out = Vec::new();
        for kind in [AudioKind::Speaker, AudioKind::Microphone] {
            assert!(!audio.open_outgoing(PEER, kind, Origin::Projection, MonoTime::ZERO, &mut out));
        }
        audio.projection_peers.insert(PEER);
        assert!(!audio.open_outgoing(
            PEER,
            AudioKind::Microphone,
            Origin::Projection,
            MonoTime::ZERO,
            &mut out
        ));
        assert!(out.is_empty() && audio.sessions.is_empty());
        assert!(audio.open_outgoing(
            PEER,
            AudioKind::Speaker,
            Origin::Projection,
            MonoTime::ZERO,
            &mut out
        ));
        assert_eq!(
            out,
            vec![Output::SendControl {
                peer: PEER,
                msg: ControlMessage::ProjectionAudioOpen {
                    stream: AudioStreamId(1)
                }
            }]
        );
    }

    #[test]
    fn a_retry_is_only_waited_for_when_it_could_fire() {
        let at = MonoTime::from_nanos(1_000_000_000);
        let armed = || {
            let (mut audio, _) = projecting(&[42]);
            audio.sources_sent.insert(PEER, BTreeSet::from([42]));
            audio.refused.insert(PEER, at);
            audio
        };
        assert_eq!(armed().next_deadline(), Some(at));
        // Each thing the retry needs, taken away: the agent is not woken for a retry that cannot
        // happen (it would be woken again and again, because the time has passed).
        type Break = fn(&mut Audio);
        let breaks: [(&str, Break); 6] = [
            ("the gate is shut", |a| a.session_permits = false),
            ("this node cannot capture", |a| a.projection_capture = false),
            ("the peer has no audio/1", |a| a.projection_peers.clear()),
            ("no pid set is in force", |a| a.sources_sent.clear()),
            ("the capture is not active", |a| a.activity.clear()),
            ("a stream already carries it", |a| {
                let mut out = Vec::new();
                a.open_outgoing(
                    PEER,
                    AudioKind::Speaker,
                    Origin::Machine,
                    MonoTime::ZERO,
                    &mut out,
                );
            }),
        ];
        for (why, apply) in breaks {
            let mut audio = armed();
            apply(&mut audio);
            // (A stream in its handshake has its own, later deadline; the retry is not it.)
            assert_ne!(audio.next_deadline(), Some(at), "{why}");
        }
    }

    #[test]
    fn the_pid_set_follows_the_view_only_for_a_connected_peer_that_accepts_it() {
        let (mut audio, view) = projecting(&[42, 7]);
        let wanted = |audio: &Audio| audio.wanted_sources(&view).get(&PEER).cloned();
        assert_eq!(wanted(&audio), Some(BTreeSet::from([7, 42])));
        // Neither `audio/1` nor the peer's `speaker` grant: nothing.
        audio.projection_peers.clear();
        assert_eq!(wanted(&audio), None);
        // The peer's `speaker` grant is enough for a legacy peer.
        audio.peer_speaker.insert(PEER);
        assert_eq!(wanted(&audio), Some(BTreeSet::from([7, 42])));
        // A peer that is not connected gets nothing, whatever it granted.
        audio.peers.get_mut(&PEER).unwrap().negotiated = false;
        assert_eq!(wanted(&audio), None);
        audio.peers.get_mut(&PEER).unwrap().negotiated = true;
        // A node that cannot capture projections never sends a set.
        audio.projection_capture = false;
        assert_eq!(wanted(&audio), None);
        // And none while the gate is shut: the host stops capturing until it reopens.
        audio.projection_capture = true;
        assert_eq!(wanted(&audio), Some(BTreeSet::from([7, 42])));
        audio.session_permits = false;
        assert_eq!(wanted(&audio), None);
    }
}
