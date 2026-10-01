//! Deterministic, sans-I/O shared audio admission. No sample data enters this module.
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crosspane_platform::{AudioDeviceError, AudioEvent, SessionEvent};
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{Capability, ControlMessage, Refusal};
use crosspane_types::audio::{AudioKind, AudioStreamId};
use crosspane_types::id::NodeId;
use crosspane_types::time::MonoTime;

use crate::io::{AudioEndpoint, AudioKey};
use crate::{Command, Failure, Input, Notice, Output};

#[derive(Debug, Default)]
struct Peer {
    negotiated: bool,
    next: u32,
    remote_high: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Handshake,
    Indicator,
    Opening,
    Active,
}

#[derive(Clone, Copy, Debug)]
struct Session {
    key: AudioKey,
    kind: AudioKind,
    incoming: bool,
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
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<MonoTime> {
        self.sessions
            .values()
            .filter(|s| s.phase != Phase::Active)
            .map(|s| s.deadline)
            .min()
    }

    // Also receives EngineGate outputs from E1's timed hotkey panic/rearm, without altering E1.
    pub(crate) fn engine_gate(&mut self, permits: bool, out: &mut Vec<Output>) {
        self.engine_permits = permits;
        if !permits {
            self.end_where(|_| true, Some(Refusal::Locked), out);
        }
    }

    pub(crate) fn handle(&mut self, input: &Input, now: MonoTime, out: &mut Vec<Output>) {
        // Expire before processing callbacks, even if the agent has not delivered Tick yet.
        self.end_where(
            |s| s.phase != Phase::Active && now >= s.deadline,
            Some(Refusal::InjectorFailed),
            out,
        );
        match input {
            Input::PeerUp { peer } => {
                if *peer != self.node {
                    self.peers.entry(*peer).or_insert_with(|| Peer {
                        next: if self.node < *peer { 1 } else { 2 },
                        ..Peer::default()
                    });
                }
            }
            Input::AudioPeer {
                peer,
                name,
                available,
            } => {
                if let Some(p) = self.peers.get_mut(peer) {
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
                        }
                    }
                }
            }
            Input::Link(LinkEvent::Closed { peer, .. }) => {
                self.end_where(|s| s.key.peer == *peer, None, out);
                self.activity.retain(|(p, _)| p != peer);
                if self.peers.remove(peer).is_some_and(|p| p.negotiated) {
                    out.push(Output::RemoveAudioPeer { peer: *peer });
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
                        !self.has_grant(s.key.peer, s.kind) && (s.incoming || removed)
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
                    self.outgoing(*peer, *kind, now, out);
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
                ControlMessage::AudioOpened { stream } => self.opened(*peer, *stream, out),
                ControlMessage::AudioClose { stream } => self.end((*peer, *stream), None, out),
                ControlMessage::AudioRefused { stream, reason } => {
                    if self
                        .sessions
                        .get(&(*peer, *stream))
                        .is_some_and(|s| !s.incoming)
                    {
                        self.end((*peer, *stream), Some(*reason), out);
                    }
                }
                ControlMessage::Grants(grants) => {
                    self.end_where(
                        |s| {
                            s.key.peer == *peer
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

    fn gate(&self) -> bool {
        self.session_permits && !self.asleep && self.engine_permits
    }
    fn connected(&self, peer: NodeId) -> bool {
        self.peers.get(&peer).is_some_and(|p| p.negotiated)
    }
    fn has_grant(&self, peer: NodeId, kind: AudioKind) -> bool {
        self.grants
            .get(&peer)
            .is_some_and(|g| g.contains(&capability(kind)))
    }
    fn admitted(&self, s: Session) -> bool {
        self.gate()
            && self.connected(s.key.peer)
            && (!s.incoming || self.has_grant(s.key.peer, s.kind))
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
    fn outgoing(&mut self, peer: NodeId, kind: AudioKind, now: MonoTime, out: &mut Vec<Output>) {
        if !self.connected(peer) {
            Self::notice(peer, kind, Refusal::Permission, out);
            return;
        }
        if !self.gate() {
            Self::notice(peer, kind, Refusal::Locked, out);
            return;
        }
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        if p.next > u16::MAX as u32 {
            Self::notice(peer, kind, Refusal::Busy, out);
            return;
        }
        let stream = AudioStreamId(p.next as u16);
        p.next += 2;
        let Some(key) = self.key(peer, stream) else {
            Self::notice(peer, kind, Refusal::Busy, out);
            return;
        };
        self.sessions.insert(
            (peer, stream),
            Session {
                key,
                kind,
                incoming: false,
                phase: Phase::Handshake,
                deadline: now.saturating_add(Duration::from_secs(2)),
            },
        );
        out.push(Output::SendControl {
            peer,
            msg: ControlMessage::AudioOpen {
                stream,
                kind,
                channels: kind.format().channels as u8,
            },
        });
    }
    fn incoming(
        &mut self,
        peer: NodeId,
        stream: AudioStreamId,
        kind: AudioKind,
        channels: u8,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let valid_id = stream.0 != 0 && (stream.0 % 2 == 1) == (peer < self.node);
        let fresh = self
            .peers
            .get(&peer)
            .is_some_and(|p| stream.0 > p.remote_high);
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
            || !self.has_grant(peer, kind)
        {
            Some(Refusal::Permission)
        } else if !self.gate() {
            Some(Refusal::Locked)
        } else if self
            .sessions
            .values()
            .any(|s| s.incoming && s.key.peer == peer && s.kind == kind)
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
                phase,
                deadline: now.saturating_add(Duration::from_secs(2)),
            },
        );
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
        audio.peers.insert(
            PEER,
            Peer {
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
            &mut out,
        );
        audio.handle(&Input::PeerUp { peer: PEER }, MonoTime::ZERO, &mut out);
        audio.handle(
            &Input::AudioPeer {
                peer: PEER,
                name: String::new(),
                available: true,
            },
            MonoTime::ZERO,
            &mut out,
        );
        out.clear();
        audio.incoming(
            PEER,
            AudioStreamId(2),
            AudioKind::Microphone,
            1,
            MonoTime::ZERO,
            &mut out,
        );
        assert!(audio.sessions.is_empty());
        assert!(out.contains(&Output::SendControl {
            peer: PEER,
            msg: ControlMessage::AudioRefused {
                stream: AudioStreamId(2),
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
}
