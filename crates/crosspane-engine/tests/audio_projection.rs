//! AUD-PROJ, "sound follows the window": the audio policy of the public engine, end to end.
//!
//! Two or three engines are wired together by routing their `SendControl` outputs, with a
//! deterministic fake of the host: windows park and capture, proxies open, playback devices open,
//! and the capture backend turns `Output::AudioProjectionSources` into `VirtualActive` (a non-empty
//! set starts the capture, an empty one stops it). Time only moves when a test moves it.
//!
//! `A` is the source (a Windows-like node that can capture projected windows) unless a test says
//! otherwise; `B` and `C` show its windows. Everybody grants everybody the window capabilities and
//! nobody grants `speaker` unless a test does, so what plays is what "sound follows the window"
//! allows.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crosspane_engine::io::{AudioEndpoint, AudioKey};
use crosspane_engine::{
    Command, Engine, EngineConfig, Input, Notice, Output, ProjectionKey, ProxyEvent,
};
use crosspane_input::journal::MemoryJournal;
use crosspane_platform::{
    AudioEvent, HotkeyEvent, LockState, Parked, ParkingKind, SessionEvent, SessionState, StreamId,
    WindowEvent, WindowInfo, WindowRole, WindowState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ControlMessage, Refusal};
use crosspane_protocol::projection::ProjectionMessage;
use crosspane_types::audio::{AudioKind, AudioStreamId};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelRect, PixelSize, PointLogical, RectLogical, SizeLogical, SizeMm,
};
use crosspane_types::id::{DisplayId, NodeId, WindowId};
use crosspane_types::time::MonoTime;

const A: NodeId = NodeId([1; 32]);
const B: NodeId = NodeId([2; 32]);
const C: NodeId = NodeId([3; 32]);
const DISPLAY: DisplayId = DisplayId(1);
const TWIN: DisplayId = DisplayId(7);
const W1: WindowId = WindowId(10);
const W2: WindowId = WindowId(11);
const W3: WindowId = WindowId(12);
const PID: u32 = 4242;
const WINDOW_CAPS: [Capability; 3] = [
    Capability::WindowShare,
    Capability::WindowBrowse,
    Capability::WindowPresent,
];
const WITH_SPEAKER: [Capability; 4] = [
    Capability::WindowShare,
    Capability::WindowBrowse,
    Capability::WindowPresent,
    Capability::AudioSpeaker,
];

fn ms(n: u64) -> MonoTime {
    MonoTime::from_nanos(n * 1_000_000)
}

fn display() -> DisplayInfo {
    DisplayInfo {
        id: DISPLAY,
        name: "fake display".into(),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(100.0, 100.0),
            pixel_size: PixelSize::new(1000, 1000),
            scale: 1.0,
            logical_origin: PointLogical::zero(),
        },
        refresh_millihz: 60_000,
        color_space: ColorSpace::Srgb,
        hdr: false,
    }
}

fn window(id: WindowId, pid: u32) -> WindowInfo {
    WindowInfo {
        id,
        title: format!("window {}", id.0),
        app_id: "fake app".into(),
        pid: Some(pid),
        display: Some(DISPLAY),
        frame: RectLogical::new(PointLogical::zero(), SizeLogical::new(400.0, 300.0)),
        state: WindowState::Normal,
        role: WindowRole::Toplevel,
        parent: None,
    }
}

fn control(peer: NodeId, msg: ControlMessage) -> Input {
    Input::Link(LinkEvent::Control { peer, msg })
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

fn pair(a: NodeId, b: NodeId) -> (NodeId, NodeId) {
    if a < b { (a, b) } else { (b, a) }
}

#[derive(Clone, Copy)]
struct Setup {
    /// Every link's `Hello`s carry `audio/1`.
    audio1: bool,
    /// The first node can capture the sound of projected windows.
    capture: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            audio1: true,
            capture: true,
        }
    }
}

/// Engines wired together, with the fake host. `feed` runs one input and everything it causes;
/// it returns the outputs of that first `Engine::handle` call, which is where "in the same
/// handle" is asserted.
struct Net {
    engines: BTreeMap<NodeId, Engine>,
    queue: VecDeque<(NodeId, Input)>,
    log: Vec<(NodeId, Output)>,
    now: u64,
    setup: Setup,
    /// Control messages between these pairs are lost (a link that is down).
    down: BTreeSet<(NodeId, NodeId)>,
    /// `OpenProxy` is not answered until the test does it.
    defer_proxy: bool,
    /// Every `ProjectionAudioOpen` is lost on the way (the destination never answers).
    lose_projection_opens: bool,
    /// Every `AudioClose` is lost on the way (the replacement race: the old stream is still open).
    lose_audio_closes: bool,
    /// Every E2 message is lost on the way (the source's view of the projection goes stale).
    lose_projection_messages: bool,
    /// Streams a test injected by hand: replies to them never reach the node that did not send
    /// them (it would answer `AudioOpened` for a stream it never opened with an `AudioClose`).
    crafted: BTreeSet<u16>,
    /// The pids each node's fake capture backend was last told to capture, per peer.
    captured: BTreeMap<(NodeId, NodeId), Vec<u32>>,
    /// Captures the fake backend reports active.
    active: BTreeSet<(NodeId, NodeId)>,
    /// Streams between `StartAudioStream` and `StopAudioStream`.
    live: BTreeMap<(NodeId, AudioKey), AudioEndpoint>,
    /// The latest speaker indicators per node.
    speakers: BTreeMap<NodeId, Vec<AudioKey>>,
    /// What each node grants each peer, as last set.
    caps: BTreeMap<(NodeId, NodeId), Vec<Capability>>,
    stream: u64,
}

impl Net {
    fn new(nodes: &[NodeId], setup: Setup) -> Self {
        let engines = nodes
            .iter()
            .map(|&node| {
                let (engine, _) = Engine::new(
                    EngineConfig::new(node),
                    Box::<MemoryJournal>::default(),
                    Box::<MemoryJournal>::default(),
                    ms(0),
                )
                .unwrap();
                (node, engine)
            })
            .collect();
        let mut net = Self {
            engines,
            queue: VecDeque::new(),
            log: Vec::new(),
            now: 0,
            setup,
            down: BTreeSet::new(),
            defer_proxy: false,
            lose_projection_opens: false,
            lose_audio_closes: false,
            lose_projection_messages: false,
            crafted: BTreeSet::new(),
            captured: BTreeMap::new(),
            active: BTreeSet::new(),
            live: BTreeMap::new(),
            speakers: BTreeMap::new(),
            caps: BTreeMap::new(),
            stream: 0,
        };
        let mut inputs = Vec::new();
        for &node in nodes {
            inputs.push((node, session(LockState::Unlocked)));
            inputs.push((node, Input::LocalDisplays(vec![display()])));
        }
        if setup.capture {
            inputs.push((nodes[0], Input::AudioProjectionCapture { available: true }));
        }
        for (i, &x) in nodes.iter().enumerate() {
            for &y in &nodes[i + 1..] {
                inputs.extend(net.link_inputs(x, y));
                net.caps.insert((x, y), WINDOW_CAPS.to_vec());
                net.caps.insert((y, x), WINDOW_CAPS.to_vec());
            }
        }
        for &node in nodes {
            inputs.push((node, net.grants_input(node)));
        }
        for (i, &x) in nodes.iter().enumerate() {
            for &y in &nodes[i + 1..] {
                for (from, to) in [(x, y), (y, x)] {
                    inputs.push((
                        to,
                        control(from, ControlMessage::Grants(WINDOW_CAPS.to_vec())),
                    ));
                }
            }
        }
        net.feed_all(inputs);
        net.log.clear();
        net
    }

    /// What each end hears when the link between `x` and `y` comes up.
    fn link_inputs(&self, x: NodeId, y: NodeId) -> Vec<(NodeId, Input)> {
        let mut inputs = Vec::new();
        for (node, peer) in [(x, y), (y, x)] {
            inputs.push((
                node,
                Input::PeerDisplays {
                    peer,
                    displays: vec![display()],
                },
            ));
            inputs.push((node, Input::PeerUp { peer }));
            inputs.push((
                node,
                Input::AudioPeer {
                    peer,
                    name: "peer".into(),
                    available: true,
                },
            ));
            if self.setup.audio1 {
                inputs.push((
                    node,
                    Input::ProjectionAudioPeer {
                        peer,
                        available: true,
                    },
                ));
            }
        }
        inputs
    }

    fn grants_input(&self, node: NodeId) -> Input {
        Input::Grants(
            self.caps
                .iter()
                .filter(|((from, _), _)| *from == node)
                .map(|((_, to), caps)| (*to, caps.iter().copied().collect()))
                .collect(),
        )
    }

    fn feed_all(&mut self, inputs: Vec<(NodeId, Input)>) {
        self.queue.extend(inputs);
        self.drain();
    }

    /// Run one input and everything it causes; the outputs of the first handle call come back.
    fn feed(&mut self, node: NodeId, input: Input) -> Vec<Output> {
        self.queue.push_back((node, input));
        self.drain().unwrap_or_default()
    }

    fn drain(&mut self) -> Option<Vec<Output>> {
        let mut first = None;
        let mut steps = 0;
        while let Some((node, input)) = self.queue.pop_front() {
            steps += 1;
            assert!(steps < 10_000, "completion loop");
            let out = self
                .engines
                .get_mut(&node)
                .unwrap()
                .handle(input, ms(self.now));
            if first.is_none() {
                first = Some(out.clone());
            }
            for output in out {
                self.complete(node, &output);
                self.log.push((node, output));
            }
        }
        first
    }

    fn deliverable(&self, from: NodeId, to: NodeId, msg: &ControlMessage) -> bool {
        if self.down.contains(&pair(from, to)) {
            return false;
        }
        match msg {
            ControlMessage::ProjectionAudioOpen { .. } if self.lose_projection_opens => false,
            ControlMessage::AudioClose { .. } if self.lose_audio_closes => false,
            ControlMessage::Projection(_) if self.lose_projection_messages => false,
            ControlMessage::AudioOpened { stream }
            | ControlMessage::AudioClose { stream }
            | ControlMessage::AudioRefused { stream, .. } => !self.crafted.contains(&stream.0),
            _ => true,
        }
    }

    fn complete(&mut self, node: NodeId, output: &Output) {
        match output {
            Output::SendControl { peer, msg } => {
                if self.deliverable(node, *peer, msg) {
                    self.queue.push_back((*peer, control(node, msg.clone())));
                }
            }
            Output::OpenProxy { key, size, .. } if !self.defer_proxy => {
                self.queue.push_back((
                    node,
                    Input::ProxyOpened {
                        key: *key,
                        result: Ok((*size, 1.0)),
                    },
                ));
            }
            Output::Park { window, size, .. } | Output::ResizeParked { window, size, .. } => {
                let content = PixelRect::new(
                    crosspane_types::geom::euclid::Point2D::new(50, 40),
                    crosspane_types::geom::euclid::Point2D::new(
                        50 + size.width as i32,
                        40 + size.height as i32,
                    ),
                );
                self.queue.push_back((
                    node,
                    Input::Parked {
                        window: *window,
                        result: Ok(Parked {
                            fullscreen: false,
                            window: *window,
                            kind: ParkingKind::Twin,
                            display: TWIN,
                            content,
                        }),
                    },
                ));
            }
            Output::StartCapture { projection, .. } => {
                self.stream += 1;
                self.queue.push_back((
                    node,
                    Input::CaptureStarted {
                        projection: *projection,
                        result: Ok(StreamId(self.stream)),
                    },
                ));
            }
            Output::ActivateWindow { window } => {
                self.queue
                    .push_back((node, Input::Windows(WindowEvent::Focused(Some(*window)))));
            }
            Output::OpenAudioPlayback { key } => {
                self.queue.push_back((
                    node,
                    Input::AudioDeviceOpened {
                        key: *key,
                        kind: AudioKind::Speaker,
                        result: Ok(()),
                    },
                ));
            }
            Output::AudioProjectionSources { peer, pids } => {
                // The host captures what it is told to and reports the capture active (or not).
                self.captured.insert((node, *peer), pids.clone());
                let wanted = !pids.is_empty();
                if wanted != self.active.contains(&(node, *peer)) {
                    if wanted {
                        self.active.insert((node, *peer));
                    } else {
                        self.active.remove(&(node, *peer));
                    }
                    self.queue.push_back((
                        node,
                        Input::Audio(AudioEvent::VirtualActive {
                            peer: *peer,
                            kind: AudioKind::Speaker,
                            active: wanted,
                        }),
                    ));
                }
            }
            Output::RemoveAudioPeer { peer } => {
                // The device and everything it captured is gone, without an event.
                self.captured.remove(&(node, *peer));
                self.active.remove(&(node, *peer));
            }
            Output::StartAudioStream { key, endpoint, .. } => {
                self.live.insert((node, *key), *endpoint);
            }
            Output::StopAudioStream { key } => {
                self.live.remove(&(node, *key));
            }
            Output::AudioIndicators { speakers, .. } => {
                self.speakers.insert(node, speakers.clone());
            }
            _ => {}
        }
    }

    // ---- scenario steps ----

    fn mark(&self) -> usize {
        self.log.len()
    }

    /// The outputs `node` produced since `mark`.
    fn since(&self, node: NodeId, mark: usize) -> Vec<Output> {
        self.log[mark..]
            .iter()
            .filter(|(n, _)| *n == node)
            .map(|(_, o)| o.clone())
            .collect()
    }

    /// The streams `node` is sending or playing right now.
    fn live(&self, node: NodeId) -> Vec<(AudioKey, AudioEndpoint)> {
        self.live
            .iter()
            .filter(|((n, _), _)| *n == node)
            .map(|((_, key), endpoint)| (*key, *endpoint))
            .collect()
    }

    fn speakers(&self, node: NodeId) -> usize {
        self.speakers.get(&node).map_or(0, Vec::len)
    }

    fn add_window(&mut self, node: NodeId, id: WindowId, pid: u32) {
        self.feed(node, Input::Windows(WindowEvent::Added(window(id, pid))));
    }

    fn started_key(&self, mark: usize, source: NodeId, peer: NodeId) -> ProjectionKey {
        self.log[mark..]
            .iter()
            .find_map(|(n, o)| match o {
                Output::Notice(Notice::ProjectionStarted { key, peer: p, .. })
                    if *n == source && *p == peer =>
                {
                    Some(*key)
                }
                _ => None,
            })
            .expect("the projection started")
    }

    /// Push `id` (a process `pid`) from `source` to `to`.
    fn project(&mut self, source: NodeId, to: NodeId, id: WindowId, pid: u32) -> ProjectionKey {
        self.add_window(source, id, pid);
        let mark = self.mark();
        self.feed(
            source,
            Input::Command(Command::Project {
                window: id,
                to,
                place: None,
            }),
        );
        self.started_key(mark, source, to)
    }

    /// Offer `id` to `to` and return the projection's key even if it has not started.
    fn offer(&mut self, source: NodeId, to: NodeId, id: WindowId, pid: u32) -> ProjectionKey {
        self.add_window(source, id, pid);
        let mark = self.mark();
        self.feed(
            source,
            Input::Command(Command::Project {
                window: id,
                to,
                place: None,
            }),
        );
        self.log[mark..]
            .iter()
            .find_map(|(n, o)| match o {
                Output::SendControl {
                    msg: ControlMessage::Projection(ProjectionMessage::Start { projection, .. }),
                    ..
                } if *n == source => Some(ProjectionKey {
                    source,
                    projection: *projection,
                }),
                _ => None,
            })
            .expect("the window was offered")
    }

    /// `destination` pulls `id` (a process `pid`) from `source`.
    fn pull(
        &mut self,
        destination: NodeId,
        source: NodeId,
        id: WindowId,
        pid: u32,
    ) -> ProjectionKey {
        self.add_window(source, id, pid);
        let mark = self.mark();
        self.feed(
            destination,
            Input::Command(Command::Pull {
                peer: source,
                window: id,
                request: 1,
            }),
        );
        self.started_key(mark, source, destination)
    }

    /// `node` now grants `peer` exactly `caps`; the peer is told, as the agent does.
    fn grant(&mut self, node: NodeId, peer: NodeId, caps: &[Capability]) -> Vec<Output> {
        self.caps.insert((node, peer), caps.to_vec());
        let input = self.grants_input(node);
        let first = self.feed(node, input);
        self.feed(peer, control(node, ControlMessage::Grants(caps.to_vec())));
        first
    }

    /// A message from `from` that the engine at `to` would not have sent itself.
    fn craft(&mut self, to: NodeId, from: NodeId, msg: ControlMessage) -> Vec<Output> {
        let stream = match &msg {
            ControlMessage::AudioOpen { stream, .. }
            | ControlMessage::ProjectionAudioOpen { stream } => stream.0,
            other => panic!("not an audio open: {other:?}"),
        };
        self.crafted.insert(stream);
        self.feed(to, control(from, msg))
    }

    fn craft_projection(&mut self, to: NodeId, from: NodeId, id: u16) -> Vec<Output> {
        self.craft(
            to,
            from,
            ControlMessage::ProjectionAudioOpen {
                stream: AudioStreamId(id),
            },
        )
    }

    fn craft_machine(&mut self, to: NodeId, from: NodeId, id: u16) -> Vec<Output> {
        self.craft(
            to,
            from,
            ControlMessage::AudioOpen {
                stream: AudioStreamId(id),
                kind: AudioKind::Speaker,
                channels: 2,
            },
        )
    }

    fn link_down(&mut self, x: NodeId, y: NodeId) {
        self.down.insert(pair(x, y));
        self.feed(x, closed(y));
        self.feed(y, closed(x));
    }

    fn link_up(&mut self, x: NodeId, y: NodeId) {
        self.down.remove(&pair(x, y));
        let mut inputs = self.link_inputs(x, y);
        for (from, to) in [(x, y), (y, x)] {
            let caps = self.caps.get(&(from, to)).cloned().unwrap_or_default();
            inputs.push((to, control(from, ControlMessage::Grants(caps))));
        }
        self.feed_all(inputs);
    }

    fn tick(&mut self, node: NodeId, at_ms: u64) -> Vec<Output> {
        self.now = at_ms;
        self.feed(node, Input::Tick)
    }
}

// ---- output readers ----

fn projection_opens(out: &[Output]) -> Vec<(NodeId, AudioStreamId)> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendControl {
                peer,
                msg: ControlMessage::ProjectionAudioOpen { stream },
            } => Some((*peer, *stream)),
            _ => None,
        })
        .collect()
}

fn machine_opens(out: &[Output]) -> Vec<(NodeId, AudioStreamId)> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendControl {
                peer,
                msg: ControlMessage::AudioOpen { stream, .. },
            } => Some((*peer, *stream)),
            _ => None,
        })
        .collect()
}

fn audio_closes(out: &[Output]) -> Vec<(NodeId, AudioStreamId)> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendControl {
                peer,
                msg: ControlMessage::AudioClose { stream },
            } => Some((*peer, *stream)),
            _ => None,
        })
        .collect()
}

fn audio_refusals(out: &[Output]) -> Vec<(AudioStreamId, Refusal)> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendControl {
                msg: ControlMessage::AudioRefused { stream, reason },
                ..
            } => Some((*stream, *reason)),
            _ => None,
        })
        .collect()
}

fn pid_sets(out: &[Output]) -> Vec<(NodeId, Vec<u32>)> {
    out.iter()
        .filter_map(|o| match o {
            Output::AudioProjectionSources { peer, pids } => Some((*peer, pids.clone())),
            _ => None,
        })
        .collect()
}

fn stops(out: &[Output]) -> usize {
    out.iter()
        .filter(|o| matches!(o, Output::StopAudioStream { .. }))
        .count()
}

fn opens_playback(out: &[Output]) -> bool {
    out.iter()
        .any(|o| matches!(o, Output::OpenAudioPlayback { .. }))
}

fn speaker_notice(out: &[Output], peer: NodeId) -> bool {
    out.contains(&Output::Notice(Notice::SpeakerInUseBy(peer)))
}

/// The one stream `node` has live, as (key, endpoint).
fn only_stream(net: &Net, node: NodeId) -> (AudioKey, AudioEndpoint) {
    let live = net.live(node);
    assert_eq!(live.len(), 1, "{node:?}: {live:?}");
    live[0]
}

/// Sound plays from `from` on `to`: one stream each, the right way round.
fn assert_sound(net: &Net, from: NodeId, to: NodeId) {
    let (_, sending) = only_stream(net, from);
    let (_, playing) = only_stream(net, to);
    assert_eq!(sending, AudioEndpoint::VirtualSpeaker);
    assert_eq!(playing, AudioEndpoint::LocalPlayback);
    assert_eq!(net.speakers(to), 1);
}

fn assert_silence(net: &Net, nodes: &[NodeId]) {
    for &node in nodes {
        assert!(net.live(node).is_empty(), "{node:?}: {:?}", net.live(node));
    }
}

fn count<T>(log: &[(NodeId, Output)], node: NodeId, f: impl Fn(&Output) -> Option<T>) -> usize {
    log.iter()
        .filter(|(n, o)| *n == node && f(o).is_some())
        .count()
}

// ---- admission: the sound follows an accepted window ----

#[test]
fn a_pushed_projection_plays_on_the_destination_without_the_speaker_grant() {
    let mut net = Net::new(&[A, B], Setup::default());
    let mark = net.mark();
    net.project(A, B, W1, PID);
    let (a, b) = (net.since(A, mark), net.since(B, mark));
    // The source captures the window's process, then opens a projection stream (never AudioOpen).
    assert_eq!(pid_sets(&a), vec![(B, vec![PID])]);
    assert_eq!(projection_opens(&a), vec![(B, AudioStreamId(1))]);
    assert!(machine_opens(&a).is_empty());
    // The destination plays it, announced like a speaker stream, with no `speaker` grant.
    assert!(speaker_notice(&b, A));
    assert!(opens_playback(&b));
    assert_sound(&net, A, B);
    assert!(
        net.caps[&(B, A)]
            .iter()
            .all(|c| *c != Capability::AudioSpeaker)
    );
}

#[test]
fn a_pulled_projection_plays_on_the_destination_without_the_speaker_grant() {
    let mut net = Net::new(&[A, B], Setup::default());
    let mark = net.mark();
    net.pull(B, A, W1, PID);
    let (a, b) = (net.since(A, mark), net.since(B, mark));
    assert_eq!(pid_sets(&a), vec![(B, vec![PID])]);
    assert_eq!(projection_opens(&a).len(), 1);
    assert!(machine_opens(&a).is_empty());
    assert!(speaker_notice(&b, A));
    assert_sound(&net, A, B);
}

#[test]
fn a_window_without_a_process_has_no_sound() {
    let mut net = Net::new(&[A, B], Setup::default());
    let mark = net.mark();
    net.project(A, B, W1, 0);
    let a = net.since(A, mark);
    assert!(pid_sets(&a).is_empty());
    assert!(projection_opens(&a).is_empty());
    assert_silence(&net, &[A, B]);
}

// ---- the sound ends, in the same handle, with the projection ----

type Trigger = Box<dyn Fn(&mut Net, ProjectionKey) -> (NodeId, Vec<Output>)>;

fn trigger(f: impl Fn(&mut Net, ProjectionKey) -> (NodeId, Vec<Output>) + 'static) -> Trigger {
    Box::new(f)
}

#[test]
fn projection_audio_ends_in_the_same_handle_as_the_projection() {
    // (cause, whether it closes the audio gate, which ends streams with a Locked notice, as before)
    let triggers: Vec<(&str, bool, Trigger)> = vec![
        (
            "destination returns the window",
            false,
            trigger(|n, k| (B, n.feed(B, Input::Command(Command::Return(k))))),
        ),
        (
            "destination closes the proxy",
            false,
            trigger(|n, k| {
                let event = ProxyEvent::CloseRequested;
                (B, n.feed(B, Input::Proxy { key: k, event }))
            }),
        ),
        (
            "destination revokes present",
            false,
            trigger(|n, _| {
                (
                    B,
                    n.grant(B, A, &[Capability::WindowShare, Capability::WindowBrowse]),
                )
            }),
        ),
        (
            "destination locks",
            true,
            trigger(|n, _| (B, n.feed(B, session(LockState::Locked)))),
        ),
        (
            "destination panics",
            true,
            trigger(|n, _| (B, n.feed(B, Input::Command(Command::Panic)))),
        ),
        (
            "source returns the window",
            false,
            trigger(|n, k| (A, n.feed(A, Input::Command(Command::Return(k))))),
        ),
        (
            "source window closes",
            false,
            trigger(|n, _| (A, n.feed(A, Input::Windows(WindowEvent::Removed(W1))))),
        ),
        (
            "source revokes share",
            false,
            trigger(|n, _| {
                (
                    A,
                    n.grant(A, B, &[Capability::WindowBrowse, Capability::WindowPresent]),
                )
            }),
        ),
        (
            "source locks",
            true,
            trigger(|n, _| (A, n.feed(A, session(LockState::Locked)))),
        ),
        (
            "source panics",
            true,
            trigger(|n, _| (A, n.feed(A, Input::Command(Command::Panic)))),
        ),
    ];
    for (name, gate, trigger) in triggers {
        let mut net = Net::new(&[A, B], Setup::default());
        let key = net.project(A, B, W1, PID);
        assert_sound(&net, A, B);
        let (node, first) = trigger(&mut net, key);
        // The node that heard it ends its end of the stream in that very call: one AudioClose, the
        // stream stopped and its indicator cleared. A projection ending is a plain close (no
        // refusal, no notice); a closed gate ends it with the usual Locked notice, as before.
        assert_eq!(audio_closes(&first).len(), 1, "{name}: {first:?}");
        assert_eq!(stops(&first), 1, "{name}: {first:?}");
        let refusal = first.iter().any(|o| {
            matches!(
                o,
                Output::Notice(Notice::AudioRefused { .. })
                    | Output::SendControl {
                        msg: ControlMessage::AudioRefused { .. },
                        ..
                    }
            )
        });
        assert_eq!(refusal, gate, "{name}: {first:?}");
        if node == B {
            assert!(
                first
                    .iter()
                    .any(|o| matches!(o, Output::CloseAudioPlayback { .. })),
                "{name}: {first:?}"
            );
            assert!(
                first.iter().any(
                    |o| matches!(o, Output::AudioIndicators { speakers, .. } if speakers.is_empty())
                ),
                "{name}: {first:?}"
            );
        } else {
            // The source also stops capturing, in the same call.
            assert_eq!(pid_sets(&first), vec![(B, vec![])], "{name}: {first:?}");
        }
        // And the other end follows once the messages arrive.
        assert_silence(&net, &[A, B]);
        assert!(
            net.captured.get(&(A, B)).is_none_or(Vec::is_empty),
            "{name}"
        );
    }
}

#[test]
fn the_reconnect_grace_expiring_leaves_no_sound_and_nothing_resumes() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    net.link_down(A, B);
    // The link loss already ended the sound at both ends.
    assert_silence(&net, &[A, B]);
    // The link returns after the grace: the projection is gone, so is any reason for sound.
    net.tick(A, 21_000);
    net.tick(B, 21_000);
    let mark = net.mark();
    net.link_up(A, B);
    let (a, b) = (net.since(A, mark), net.since(B, mark));
    assert!(pid_sets(&a).iter().all(|(_, pids)| pids.is_empty()));
    assert!(projection_opens(&a).is_empty());
    assert!(!speaker_notice(&b, A));
    assert_silence(&net, &[A, B]);
}

#[test]
fn a_resumed_projection_brings_the_sound_back_only_once_accepted_again() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    net.link_down(A, B);
    assert_silence(&net, &[A, B]);
    assert!(!net.captured.contains_key(&(A, B)));
    net.now = 5_000;
    let mark = net.mark();
    net.link_up(A, B);
    // The set returns after the destination accepted the resumed projection, never before.
    let accepted = net.log[mark..]
        .iter()
        .position(|(n, o)| {
            *n == B
                && matches!(
                    o,
                    Output::SendControl {
                        msg: ControlMessage::Projection(ProjectionMessage::Accepted { .. }),
                        ..
                    }
                )
        })
        .expect("the destination accepted the resumed projection");
    let set = net.log[mark..]
        .iter()
        .position(|(n, o)| {
            *n == A && matches!(o, Output::AudioProjectionSources { pids, .. } if !pids.is_empty())
        })
        .expect("the source captures again");
    assert!(accepted < set, "{accepted} < {set}");
    // The capture was re-added, so the host was told again, and a fresh stream opened.
    assert_eq!(net.captured[&(A, B)], vec![PID]);
    assert_sound(&net, A, B);
    let a = net.since(A, mark);
    assert_eq!(projection_opens(&a), vec![(B, AudioStreamId(3))]);
}

#[test]
fn the_destination_refuses_the_stream_while_the_resumed_projection_is_not_back() {
    let mut net = Net::new(
        &[A, B],
        Setup {
            capture: false,
            ..Setup::default()
        },
    );
    net.project(A, B, W1, PID);
    net.link_down(A, B);
    // B's link is back (and the audio feature), but A has not re-offered the window yet: the
    // destination is suspended, so nothing plays.
    let only_b = net
        .link_inputs(A, B)
        .into_iter()
        .filter(|(n, _)| *n == B)
        .collect();
    net.feed_all(only_b);
    let first = net.craft_projection(B, A, 101);
    assert_eq!(
        audio_refusals(&first),
        vec![(AudioStreamId(101), Refusal::Permission)]
    );
    assert!(!opens_playback(&first));
    // A hears the link, re-offers the window, B un-suspends it: now it plays.
    net.down.remove(&pair(A, B));
    let only_a = net
        .link_inputs(A, B)
        .into_iter()
        .filter(|(n, _)| *n == A)
        .collect();
    net.feed_all(only_a);
    let first = net.craft_projection(B, A, 103);
    assert!(opens_playback(&first), "{first:?}");
    assert!(speaker_notice(&first, A));
}

// ---- the destination's admission rules (D1) ----

fn no_capture() -> Setup {
    Setup {
        capture: false,
        ..Setup::default()
    }
}

#[test]
fn no_live_projection_refuses_permission() {
    let mut net = Net::new(&[A, B], no_capture());
    let first = net.craft_projection(B, A, 101);
    assert_eq!(
        audio_refusals(&first),
        vec![(AudioStreamId(101), Refusal::Permission)]
    );
    assert!(!opens_playback(&first));
    assert!(!speaker_notice(&first, A));
    assert_silence(&net, &[B]);
}

#[test]
fn a_speaker_grant_alone_does_not_admit_a_projection_stream() {
    let mut net = Net::new(&[A, B], no_capture());
    net.grant(B, A, &WITH_SPEAKER);
    let first = net.craft_projection(B, A, 101);
    assert_eq!(
        audio_refusals(&first),
        vec![(AudioStreamId(101), Refusal::Permission)]
    );
    // ... while the same grant admits the whole-machine stream.
    let first = net.craft_machine(B, A, 103);
    assert!(opens_playback(&first), "{first:?}");
}

#[test]
fn an_unopened_proxy_refuses_permission_until_it_opens() {
    let mut net = Net::new(&[A, B], no_capture());
    net.defer_proxy = true;
    let key = net.offer(A, B, W1, PID);
    let first = net.craft_projection(B, A, 101);
    assert_eq!(
        audio_refusals(&first),
        vec![(AudioStreamId(101), Refusal::Permission)]
    );
    // The proxy opens (and the projection is accepted): the next stream is admitted.
    net.feed(
        B,
        Input::ProxyOpened {
            key,
            result: Ok((PixelSize::new(400, 300), 1.0)),
        },
    );
    let first = net.craft_projection(B, A, 103);
    assert!(opens_playback(&first), "{first:?}");
}

#[test]
fn a_peer_without_audio1_is_refused_even_with_a_shown_projection() {
    let setup = Setup {
        audio1: false,
        capture: false,
    };
    let mut net = Net::new(&[A, B], setup);
    net.project(A, B, W1, PID);
    let first = net.craft_projection(B, A, 101);
    assert_eq!(
        audio_refusals(&first),
        vec![(AudioStreamId(101), Refusal::Permission)]
    );
    assert!(!opens_playback(&first));
    // Negotiating the feature admits it; losing it ends the admitted stream in that very call.
    net.feed(
        B,
        Input::ProjectionAudioPeer {
            peer: A,
            available: true,
        },
    );
    let first = net.craft_projection(B, A, 103);
    assert!(opens_playback(&first), "{first:?}");
    assert_eq!(net.live(B).len(), 1);
    let first = net.feed(
        B,
        Input::ProjectionAudioPeer {
            peer: A,
            available: false,
        },
    );
    assert_eq!(audio_closes(&first), vec![(A, AudioStreamId(103))]);
    assert_eq!(stops(&first), 1);
    assert_silence(&net, &[B]);
}

#[test]
fn a_closed_audio_gate_refuses_locked_and_a_reopened_one_admits() {
    let mut net = Net::new(&[A, B], no_capture());
    net.project(A, B, W1, PID);
    // The timed hotkey panic closes only the audio gate; E2 keeps the projection.
    net.feed(B, Input::Hotkey(HotkeyEvent::Pressed { at: ms(0) }));
    net.tick(B, 1_000);
    let first = net.craft_projection(B, A, 101);
    assert_eq!(
        audio_refusals(&first),
        vec![(AudioStreamId(101), Refusal::Locked)]
    );
    net.feed(B, Input::Command(Command::Rearm));
    let first = net.craft_projection(B, A, 103);
    assert!(opens_playback(&first), "{first:?}");
}

#[test]
fn stream_ids_must_be_valid_fresh_and_one_projection_stream_per_peer() {
    let mut net = Net::new(&[A, B], no_capture());
    net.project(A, B, W1, PID);
    // Zero, the wrong parity (A < B, so A's IDs are odd) and a stale ID are refused.
    for id in [0, 2] {
        let first = net.craft_projection(B, A, id);
        assert_eq!(
            audio_refusals(&first),
            vec![(AudioStreamId(id), Refusal::Permission)]
        );
    }
    let first = net.craft_projection(B, A, 101);
    assert!(opens_playback(&first), "{first:?}");
    // A second one from the same peer is Busy ...
    let first = net.craft_projection(B, A, 103);
    assert_eq!(
        audio_refusals(&first),
        vec![(AudioStreamId(103), Refusal::Busy)]
    );
    // ... a replay of an ID is refused and ends the stream it names ...
    let first = net.craft_projection(B, A, 101);
    assert_eq!(
        audio_refusals(&first),
        vec![(AudioStreamId(101), Refusal::Permission)]
    );
    assert_eq!(stops(&first), 1);
    assert_silence(&net, &[B]);
    // ... and the next fresh ID is admitted again.
    let first = net.craft_projection(B, A, 105);
    assert!(opens_playback(&first), "{first:?}");
}

// ---- whole-machine audio is untouched (D2) and separate ----

#[test]
fn whole_machine_audio_still_needs_the_speaker_grant_while_a_window_projects() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    assert_sound(&net, A, B);
    let first = net.craft_machine(B, A, 101);
    assert_eq!(
        audio_refusals(&first),
        vec![(AudioStreamId(101), Refusal::Permission)]
    );
    assert!(!opens_playback(&first));
    // The projection stream is untouched.
    assert_eq!(net.live(B).len(), 1);
    // With the grant, the machine stream is admitted next to it.
    net.grant(B, A, &WITH_SPEAKER);
    let first = net.craft_machine(B, A, 103);
    assert!(opens_playback(&first), "{first:?}");
    assert_eq!(net.live(B).len(), 2);
    assert_eq!(net.speakers(B), 2);
}

#[test]
fn projection_and_whole_machine_streams_from_one_peer_have_separate_busy_slots() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.grant(B, A, &WITH_SPEAKER);
    net.project(A, B, W1, PID);
    let first = net.craft_machine(B, A, 101);
    assert!(opens_playback(&first), "{first:?}");
    assert_eq!(net.live(B).len(), 2);
    // Each slot is taken: a second of either kind is Busy, and does not touch the other.
    let first = net.craft_machine(B, A, 103);
    assert_eq!(
        audio_refusals(&first),
        vec![(AudioStreamId(103), Refusal::Busy)]
    );
    let first = net.craft_projection(B, A, 105);
    assert_eq!(
        audio_refusals(&first),
        vec![(AudioStreamId(105), Refusal::Busy)]
    );
    assert_eq!(net.live(B).len(), 2);
    // Closing the machine stream leaves the projection stream, and the other way round.
    let first = net.feed(
        B,
        control(
            A,
            ControlMessage::AudioClose {
                stream: AudioStreamId(101),
            },
        ),
    );
    assert_eq!(stops(&first), 1);
    assert_sound(&net, A, B);
}

#[test]
fn revoking_the_local_speaker_grant_ends_the_machine_stream_not_the_projection_stream() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.grant(B, A, &WITH_SPEAKER);
    net.project(A, B, W1, PID);
    net.craft_machine(B, A, 101);
    assert_eq!(net.live(B).len(), 2);
    let mark = net.mark();
    let first = net.grant(B, A, &WINDOW_CAPS);
    // Exactly the machine stream (101) ended; the projection stream (1) did not.
    assert_eq!(audio_closes(&first), vec![(A, AudioStreamId(101))]);
    assert_eq!(stops(&first), 1);
    assert!(
        audio_closes(&net.since(B, mark))
            .iter()
            .all(|(_, s)| s.0 == 101)
    );
    assert_sound(&net, A, B);
}

#[test]
fn revoking_a_speaker_grant_that_was_never_needed_does_not_touch_projection_audio() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    let mark = net.mark();
    let first = net.grant(B, A, &[Capability::WindowShare, Capability::WindowPresent]);
    assert!(audio_closes(&first).is_empty());
    assert_eq!(stops(&first), 0);
    assert!(audio_closes(&net.since(A, mark)).is_empty());
    assert_sound(&net, A, B);
}

// ---- the source: who gets the pids, and when a stream opens (S1-S5) ----

#[test]
fn a_legacy_peer_needs_the_speaker_grant_and_gets_the_whole_machine_stream() {
    let setup = Setup {
        audio1: false,
        capture: true,
    };
    let mut net = Net::new(&[A, B], setup);
    // Without the peer's `speaker` grant the source sends it no pids and opens nothing.
    let mark = net.mark();
    net.project(A, B, W1, PID);
    let a = net.since(A, mark);
    assert!(
        pid_sets(&a).iter().all(|(_, pids)| pids.is_empty()),
        "{a:?}"
    );
    assert!(projection_opens(&a).is_empty() && machine_opens(&a).is_empty());
    assert_silence(&net, &[A, B]);
    // The peer grants `speaker` (and tells us, as the agent does): the pids flow and the legacy
    // `AudioOpen` is used, never the projection message.
    let mark = net.mark();
    net.grant(B, A, &WITH_SPEAKER);
    let (a, b) = (net.since(A, mark), net.since(B, mark));
    assert_eq!(pid_sets(&a), vec![(B, vec![PID])]);
    assert_eq!(machine_opens(&a), vec![(B, AudioStreamId(1))]);
    assert!(projection_opens(&a).is_empty());
    assert!(speaker_notice(&b, A));
    assert_sound(&net, A, B);
    // Revoking it ends the stream and empties the pid set, as before.
    let mark = net.mark();
    net.grant(B, A, &WINDOW_CAPS);
    let a = net.since(A, mark);
    assert_eq!(pid_sets(&a), vec![(B, vec![])]);
    assert_silence(&net, &[A, B]);
    assert!(
        !net.log
            .iter()
            .any(|(_, o)| !projection_opens(std::slice::from_ref(o)).is_empty())
    );
}

#[test]
fn a_peer_that_ends_its_speaker_grant_cuts_a_legacy_stream_but_not_a_projection_stream() {
    // Legacy: the peer's `Grants` message without `speaker` ends the stream (the old rule).
    let setup = Setup {
        audio1: false,
        capture: true,
    };
    let mut net = Net::new(&[A, B], setup);
    net.grant(B, A, &WITH_SPEAKER);
    net.project(A, B, W1, PID);
    assert_sound(&net, A, B);
    let first = net.feed(A, control(B, ControlMessage::Grants(WINDOW_CAPS.to_vec())));
    assert_eq!(audio_closes(&first).len(), 1);
    assert_eq!(pid_sets(&first), vec![(B, vec![])]);
    // audio/1: the same message changes nothing, whatever it says about `speaker`.
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    for caps in [vec![], WINDOW_CAPS.to_vec(), WITH_SPEAKER.to_vec()] {
        let first = net.feed(A, control(B, ControlMessage::Grants(caps)));
        assert!(audio_closes(&first).is_empty(), "{first:?}");
        assert!(pid_sets(&first).is_empty(), "{first:?}");
        assert_sound(&net, A, B);
    }
}

#[test]
fn a_node_that_cannot_capture_projections_behaves_as_before() {
    let mut net = Net::new(&[A, B], no_capture());
    let mark = net.mark();
    net.project(A, B, W1, PID);
    let a = net.since(A, mark);
    assert!(pid_sets(&a).is_empty());
    assert!(projection_opens(&a).is_empty());
    // A whole-machine virtual speaker reporting activity opens the legacy stream, even to a peer
    // with audio/1, and the peer still needs the `speaker` grant for it.
    let first = net.feed(
        A,
        Input::Audio(AudioEvent::VirtualActive {
            peer: B,
            kind: AudioKind::Speaker,
            active: true,
        }),
    );
    assert_eq!(machine_opens(&first), vec![(B, AudioStreamId(1))]);
    assert!(projection_opens(&first).is_empty());
    assert_silence(&net, &[A, B]);
}

#[test]
fn losing_audio1_ends_the_stream_at_both_ends_and_regaining_it_restores_the_sound() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    assert_sound(&net, A, B);
    let first_a = audio1(&mut net, A, B, false);
    // The source stops in that call: plain close, and (no `speaker` grant) the pids withdrawn.
    assert_eq!(audio_closes(&first_a).len(), 1);
    assert_eq!(pid_sets(&first_a), vec![(B, vec![])]);
    let first_b = audio1(&mut net, B, A, false);
    assert!(
        audio_closes(&first_b).is_empty(),
        "the stream is already gone: {first_b:?}"
    );
    assert_silence(&net, &[A, B]);
    // Both negotiate it again: the pids return and a new stream opens.
    audio1(&mut net, B, A, true);
    audio1(&mut net, A, B, true);
    assert_sound(&net, A, B);
}

// ---- level triggering: replacement, refusal, no storms ----

#[test]
fn a_replaced_connection_reopens_the_stream_once_the_peer_announces_audio1_again() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    let (old, _) = only_stream(&net, A);
    let mark = net.mark();
    let first = net.feed(A, Input::AudioConnectionReplaced { peer: B });
    // In one call: the old stream ends and the set is withdrawn. Nothing is asked of the new
    // connection, whose `Hello` nobody has looked at yet.
    assert_eq!(audio_closes(&first), vec![(B, old.stream)]);
    assert!(projection_opens(&first).is_empty());
    assert_eq!(pid_sets(&first), vec![(B, vec![])]);
    assert_silence(&net, &[A, B]);
    assert!(projection_opens(&net.since(A, mark)).is_empty());
    // The agent finds `audio/1` in the new `Hello` and announces it: the set returns and a new
    // stream is asked for, with the next stream ID, and the destination plays it.
    let first = audio1(&mut net, A, B, true);
    assert_eq!(pid_sets(&first), vec![(B, vec![PID])]);
    assert_eq!(
        projection_opens(&net.since(A, mark)),
        vec![(B, AudioStreamId(old.stream.0 + 2))]
    );
    assert_sound(&net, A, B);
    let (new, _) = only_stream(&net, A);
    assert!(new.generation > old.generation);
}

#[test]
fn a_replaced_connection_whose_hello_lacks_audio1_is_never_sent_a_projection_stream() {
    // The agent feeds `AudioConnectionReplaced` before it has looked at the new `Hello`. When
    // that `Hello` no longer carries `audio/1` it never announces the feature again, and a
    // `ProjectionAudioOpen` on that connection would break "never sent unless negotiated".
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    assert_sound(&net, A, B);
    let mark = net.mark();
    let first = net.feed(A, Input::AudioConnectionReplaced { peer: B });
    assert_eq!(audio_closes(&first).len(), 1);
    assert!(projection_opens(&first).is_empty(), "{first:?}");
    assert_eq!(pid_sets(&first), vec![(B, vec![])]);
    // Nothing later opens one either: the passing of time, a changed pid set, many retry periods.
    net.tick(A, 10_000);
    net.feed(A, Input::Windows(WindowEvent::Changed(window(W1, 43))));
    net.tick(A, 400_000);
    let a = net.since(A, mark);
    assert!(projection_opens(&a).is_empty(), "{a:?}");
    assert!(machine_opens(&a).is_empty(), "{a:?}");
    assert_eq!(
        pid_sets(&a),
        vec![(B, vec![])],
        "no set while it is withdrawn"
    );
    assert_silence(&net, &[A, B]);
    // Only the announcement (what the agent feeds when the new `Hello` does carry it) brings the
    // sound back.
    audio1(&mut net, A, B, true);
    assert_sound(&net, A, B);
}

fn audio1(net: &mut Net, node: NodeId, peer: NodeId, available: bool) -> Vec<Output> {
    net.feed(node, Input::ProjectionAudioPeer { peer, available })
}

#[test]
fn a_peer_that_closes_the_stream_is_not_asked_again_until_something_changes() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    let opens = |net: &Net| {
        count(&net.log, A, |o| {
            projection_opens(std::slice::from_ref(o)).first().copied()
        })
    };
    assert_eq!(opens(&net), 1);
    // B drops the feature: it ends the stream and tells A, which marks B as refusing.
    audio1(&mut net, B, A, false);
    assert_silence(&net, &[A, B]);
    assert_eq!(opens(&net), 1);
    // Inputs that change neither the pid set nor the capture do not make A ask again, including a
    // second window of the same process.
    net.tick(A, 3_000);
    net.project(A, B, W2, PID);
    net.add_window(A, W3, 99);
    assert_eq!(opens(&net), 1, "no storm");
    // A new pid changes the set: A asks once more, B refuses (it has no audio/1) and A stops.
    let mark = net.mark();
    net.feed(A, Input::Windows(WindowEvent::Changed(window(W2, 43))));
    assert_eq!(pid_sets(&net.since(A, mark)), vec![(B, vec![43, PID])]);
    assert_eq!(opens(&net), 2);
    net.tick(A, 4_000);
    net.tick(B, 4_000);
    assert_eq!(opens(&net), 2, "refused again, not asked again");
    // The capture going inactive and active again is an edge and asks once more.
    for active in [false, true] {
        net.feed(
            A,
            Input::Audio(AudioEvent::VirtualActive {
                peer: B,
                kind: AudioKind::Speaker,
                active,
            }),
        );
    }
    assert_eq!(opens(&net), 3);
    // B has the feature back (both ends agree again): the sound returns.
    audio1(&mut net, A, B, false);
    audio1(&mut net, B, A, true);
    audio1(&mut net, A, B, true);
    assert_sound(&net, A, B);
}

fn opens_by(net: &Net, node: NodeId) -> usize {
    count(&net.log, node, |o| {
        projection_opens(std::slice::from_ref(o)).first().copied()
    })
}

fn next_deadline(net: &Net, node: NodeId) -> Option<MonoTime> {
    net.engines[&node].next_deadline()
}

#[test]
fn a_destination_that_never_answers_is_asked_at_growing_intervals() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.lose_projection_opens = true;
    net.project(A, B, W1, PID);
    assert_eq!(opens_by(&net, A), 1);
    // The handshake times out: the source gives up the stream (a refusal notice, a close) ...
    let first = net.tick(A, 2_100);
    assert_eq!(audio_closes(&first).len(), 1);
    assert!(first.iter().any(|o| matches!(
        o,
        Output::Notice(Notice::AudioRefused {
            reason: Refusal::InjectorFailed,
            ..
        })
    )));
    // ... and leaves the peer alone for the first retry period (5 s), which is the next thing
    // it waits for.
    assert_eq!(next_deadline(&net, A), Some(ms(7_100)));
    net.tick(A, 4_500);
    assert_eq!(opens_by(&net, A), 1, "no storm");
    // The retry fires on the first tick at or after that time, and its own timeout starts the
    // next backoff from there, twice as long (10 s).
    net.tick(A, 7_100);
    assert_eq!(opens_by(&net, A), 2);
    net.tick(A, 9_200);
    assert_eq!(opens_by(&net, A), 2);
    assert_eq!(next_deadline(&net, A), Some(ms(19_200)));
    net.tick(A, 14_200);
    assert_eq!(opens_by(&net, A), 2, "the old, shorter period is gone");
    net.tick(A, 19_199);
    assert_eq!(opens_by(&net, A), 2);
    net.tick(A, 19_200);
    assert_eq!(opens_by(&net, A), 3);
    // And the one after that is twice again (20 s).
    net.tick(A, 21_300);
    assert_eq!(next_deadline(&net, A), Some(ms(41_300)));
}

#[test]
fn a_timed_hotkey_panic_on_the_source_stops_the_capture_and_rearming_restores_it() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    assert_eq!(net.captured[&(A, B)], vec![PID]);
    net.feed(A, Input::Hotkey(HotkeyEvent::Pressed { at: ms(0) }));
    let first = net.tick(A, 1_000);
    assert!(first.contains(&Output::EngineGate(false)));
    assert_eq!(audio_closes(&first).len(), 1);
    // The gate is shut, so the host stops capturing the window's process rather than keep
    // recording it with nowhere to send it: the set is withdrawn in that very call.
    assert_eq!(pid_sets(&first), vec![(B, vec![])]);
    assert!(net.captured[&(A, B)].is_empty());
    assert_silence(&net, &[A, B]);
    // E2 kept the projection, so the rearm sends the set again, the capture starts, and the
    // stream reopens (a new ID) with no other input.
    let mark = net.mark();
    let first = net.feed(A, Input::Command(Command::Rearm));
    assert_eq!(pid_sets(&first), vec![(B, vec![PID])]);
    assert_eq!(net.captured[&(A, B)], vec![PID]);
    assert_eq!(
        projection_opens(&net.since(A, mark)),
        vec![(B, AudioStreamId(3))]
    );
    assert_sound(&net, A, B);
}

#[test]
fn a_lock_withdraws_the_capture_with_the_stream_and_nothing_re_sends_it_while_shut() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    let first = net.feed(A, session(LockState::Locked));
    assert_eq!(pid_sets(&first), vec![(B, vec![])]);
    assert!(net.captured[&(A, B)].is_empty());
    let mark = net.mark();
    net.tick(A, 10_000);
    assert!(pid_sets(&net.since(A, mark)).is_empty());
}

#[test]
fn sound_resumes_by_itself_after_the_destination_panics_and_rearms() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    // B's timed hotkey panic closes only its audio gate: its stream ends and A is told.
    net.feed(B, Input::Hotkey(HotkeyEvent::Pressed { at: ms(0) }));
    let first = net.tick(B, 1_000);
    assert!(first.contains(&Output::EngineGate(false)));
    assert_silence(&net, &[A, B]);
    // B rearms: A has to be asked again, and it was told to wait one retry period.
    net.feed(B, Input::Command(Command::Rearm));
    assert_silence(&net, &[A, B]);
    assert_eq!(next_deadline(&net, A), Some(ms(6_000)));
    // Nothing but A's own tick brings the sound back, at that time and not before.
    net.tick(A, 5_999);
    assert_silence(&net, &[A, B]);
    net.tick(A, 6_000);
    assert_sound(&net, A, B);
    // The retry is over: nothing is left to wait for.
    assert!(!matches!(next_deadline(&net, A), Some(at) if at <= ms(6_000)));
}

#[test]
fn a_busy_refusal_in_the_replacement_race_is_retried() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.project(A, B, W1, PID);
    // The connection is replaced and A hears first: its old stream's close is still on the way
    // when the new stream's open (sent as soon as A's agent announces `audio/1` on the new
    // connection) overtakes it, so B (which still has the old one) says Busy.
    net.lose_audio_closes = true;
    let mark = net.mark();
    let first = net.feed(A, Input::AudioConnectionReplaced { peer: B });
    assert_eq!(audio_closes(&first), vec![(B, AudioStreamId(1))]);
    assert!(projection_opens(&first).is_empty());
    audio1(&mut net, A, B, true);
    assert_eq!(
        projection_opens(&net.since(A, mark)),
        vec![(B, AudioStreamId(3))]
    );
    assert_eq!(
        audio_refusals(&net.since(B, mark)),
        vec![(AudioStreamId(3), Refusal::Busy)]
    );
    // B hears the replacement too and ends the old stream, then announces `audio/1` on the new
    // connection: silence, A is backed off.
    net.lose_audio_closes = false;
    net.feed(B, Input::AudioConnectionReplaced { peer: A });
    audio1(&mut net, B, A, true);
    assert_silence(&net, &[A, B]);
    assert_eq!(opens_by(&net, A), 2);
    assert_eq!(next_deadline(&net, A), Some(ms(5_000)));
    // A asks again when the backoff is over and is admitted.
    net.tick(A, 4_999);
    assert_silence(&net, &[A, B]);
    net.tick(A, 5_000);
    assert_eq!(opens_by(&net, A), 3);
    assert_sound(&net, A, B);
}

#[test]
fn a_peer_that_keeps_refusing_is_asked_at_doubling_intervals_up_to_five_minutes() {
    let mut net = Net::new(&[A, B], Setup::default());
    let key = net.project(A, B, W1, PID);
    // B returns the window, but everything E2 says about it is lost: A still believes B shows
    // it, so it keeps its pid set and its capture. B's AudioClose is the only thing that arrives.
    net.lose_projection_messages = true;
    net.feed(B, Input::Command(Command::Return(key)));
    assert_silence(&net, &[A, B]);
    assert_eq!(opens_by(&net, A), 1);
    assert_eq!(net.captured[&(A, B)], vec![PID], "A's view is stale");
    assert_eq!(next_deadline(&net, A), Some(ms(5_000)));
    // B has no projection from A, so every retry is refused (Permission) and backs off longer:
    // the gaps are 5, 10, 20, 40, 80 and 160 s, then 300 s for as long as it keeps refusing.
    let mut at = Vec::new();
    for _ in 0..9 {
        let due = next_deadline(&net, A).expect("a retry is waited for");
        let due = due.as_nanos() / 1_000_000;
        let before = opens_by(&net, A);
        net.tick(A, due - 1);
        assert_eq!(opens_by(&net, A), before, "not before its time ({due})");
        net.tick(A, due);
        assert_eq!(opens_by(&net, A), before + 1, "asked at {due}");
        at.push(due);
    }
    assert_eq!(
        at,
        vec![
            5_000, 15_000, 35_000, 75_000, 155_000, 315_000, 615_000, 915_000, 1_215_000
        ]
    );
    assert_silence(&net, &[A, B]);
}

#[test]
fn a_stream_the_peer_accepts_starts_the_backoff_over() {
    let mut net = Net::new(&[A, B], Setup::default());
    net.lose_projection_opens = true;
    net.project(A, B, W1, PID);
    // Two timeouts in a row: the waits are 5 s and then 10 s.
    net.tick(A, 2_100);
    net.tick(A, 7_100);
    net.tick(A, 9_200);
    assert_eq!(next_deadline(&net, A), Some(ms(19_200)));
    // The next retry gets through and is accepted.
    net.lose_projection_opens = false;
    net.tick(A, 19_200);
    assert_sound(&net, A, B);
    // When B ends the stream later (it loses `audio/1`), the wait is the first period again, not
    // the 20 s that the two earlier refusals would make it.
    audio1(&mut net, B, A, false);
    assert_silence(&net, &[A, B]);
    assert_eq!(next_deadline(&net, A), Some(ms(24_200)));
}

#[test]
fn a_pid_set_change_starts_the_backoff_over_and_not_only_the_wait() {
    let mut net = Net::new(&[A, B], Setup::default());
    let key = net.project(A, B, W1, PID);
    net.lose_projection_messages = true;
    net.feed(B, Input::Command(Command::Return(key)));
    assert_eq!(next_deadline(&net, A), Some(ms(5_000)));
    // The retry is refused: the next wait is 10 s.
    net.tick(A, 5_000);
    assert_eq!(next_deadline(&net, A), Some(ms(15_000)));
    // A new process in the set: asked at once, and if it is refused again the wait is the first
    // period (5 s), not the 20 s the three refusals would make it.
    net.tick(A, 6_000);
    let before = opens_by(&net, A);
    net.feed(A, Input::Windows(WindowEvent::Changed(window(W1, 43))));
    assert_eq!(opens_by(&net, A), before + 1);
    assert_eq!(next_deadline(&net, A), Some(ms(11_000)));
}

#[test]
fn a_pid_set_change_or_a_new_activity_edge_still_ends_the_backoff_at_once() {
    let mut net = Net::new(&[A, B], Setup::default());
    let key = net.project(A, B, W1, PID);
    net.lose_projection_messages = true;
    net.feed(B, Input::Command(Command::Return(key)));
    assert_eq!(opens_by(&net, A), 1);
    // A new process in the set: asked again now, not in five seconds.
    net.feed(A, Input::Windows(WindowEvent::Changed(window(W1, 43))));
    assert_eq!(opens_by(&net, A), 2);
    // The capture going inactive and active again: asked again now.
    for active in [false, true] {
        net.feed(
            A,
            Input::Audio(AudioEvent::VirtualActive {
                peer: B,
                kind: AudioKind::Speaker,
                active,
            }),
        );
    }
    assert_eq!(opens_by(&net, A), 3);
}

// ---- several windows, several peers ----

#[test]
fn two_windows_of_one_process_share_one_stream_until_the_last_one_ends() {
    let mut net = Net::new(&[A, B], Setup::default());
    let k1 = net.project(A, B, W1, PID);
    let mark = net.mark();
    let k2 = net.project(A, B, W2, PID);
    // The second window changes nothing: same set, same stream.
    let a = net.since(A, mark);
    assert!(pid_sets(&a).is_empty());
    assert!(projection_opens(&a).is_empty());
    assert_sound(&net, A, B);
    // A window of another process joins: the set grows (sorted), the stream stays.
    net.add_window(A, W3, 43);
    let mark = net.mark();
    let k3 = net.project(A, B, W3, 43);
    assert_eq!(pid_sets(&net.since(A, mark)), vec![(B, vec![43, PID])]);
    assert!(projection_opens(&net.since(A, mark)).is_empty());
    assert_sound(&net, A, B);
    // Ending them one by one: the stream goes with the last window of the last process.
    let first = net.feed(A, Input::Command(Command::Return(k3)));
    assert_eq!(pid_sets(&first), vec![(B, vec![PID])]);
    assert!(audio_closes(&first).is_empty());
    assert_sound(&net, A, B);
    let first = net.feed(A, Input::Command(Command::Return(k1)));
    assert!(
        pid_sets(&first).is_empty() && audio_closes(&first).is_empty(),
        "{first:?}"
    );
    assert_sound(&net, A, B);
    let first = net.feed(A, Input::Command(Command::Return(k2)));
    assert_eq!(pid_sets(&first), vec![(B, vec![])]);
    assert_eq!(audio_closes(&first).len(), 1);
    assert_silence(&net, &[A, B]);
}

#[test]
fn one_process_projected_to_two_peers_has_an_independent_stream_each() {
    let mut net = Net::new(&[A, B, C], Setup::default());
    let to_b = net.project(A, B, W1, PID);
    let mark = net.mark();
    net.project(A, C, W2, PID);
    let a = net.since(A, mark);
    assert_eq!(pid_sets(&a), vec![(C, vec![PID])]);
    assert_eq!(projection_opens(&a), vec![(C, AudioStreamId(1))]);
    assert_eq!(net.live(A).len(), 2);
    assert_eq!(net.live(B).len(), 1);
    assert_eq!(net.live(C).len(), 1);
    // Ending one peer's window ends that peer's stream only.
    let first = net.feed(A, Input::Command(Command::Return(to_b)));
    assert_eq!(pid_sets(&first), vec![(B, vec![])]);
    assert_eq!(audio_closes(&first).len(), 1);
    assert_eq!(net.live(A).len(), 1);
    assert_silence(&net, &[B]);
    assert_eq!(net.live(C).len(), 1);
    // One peer dropping out does not touch another's stream: B gets a window again, then C loses
    // audio/1 (and its stream) while B keeps playing.
    net.project(A, B, W3, 43);
    assert_eq!(net.live(B).len(), 1);
    audio1(&mut net, C, A, false);
    assert_silence(&net, &[C]);
    assert_eq!(net.live(B).len(), 1);
    assert_eq!(net.live(A).len(), 1);
}

#[test]
fn each_peer_gets_the_stream_its_version_understands() {
    // B has audio/1; C is an old build that granted `speaker`.
    let mut net = Net::new(&[A, B, C], Setup::default());
    audio1(&mut net, A, C, false);
    audio1(&mut net, C, A, false);
    net.grant(C, A, &WITH_SPEAKER);
    let mark = net.mark();
    net.project(A, B, W1, PID);
    net.project(A, C, W2, PID);
    let a = net.since(A, mark);
    assert_eq!(projection_opens(&a), vec![(B, AudioStreamId(1))]);
    assert_eq!(machine_opens(&a), vec![(C, AudioStreamId(1))]);
    assert_eq!(pid_sets(&a), vec![(B, vec![PID]), (C, vec![PID])], "{a:?}");
    assert_eq!(net.live(A).len(), 2);
    assert_eq!(net.live(B).len(), 1);
    assert_eq!(net.live(C).len(), 1);
    // B never granted `speaker`, C did: only C's stream depends on it.
    net.grant(C, A, &WINDOW_CAPS);
    assert_silence(&net, &[C]);
    assert_eq!(net.live(B).len(), 1);
}

// ---- lock, panic, rearm ----

#[test]
fn lock_panic_and_rearm_end_the_sound_and_nothing_restarts_until_projected_again() {
    // Destination and source each lock; unlocking restarts nothing, projecting again does.
    for node in [B, A] {
        let mut net = Net::new(&[A, B], Setup::default());
        net.project(A, B, W1, PID);
        let first = net.feed(node, session(LockState::Locked));
        assert_eq!(audio_closes(&first).len(), 1, "{node:?}: {first:?}");
        assert_silence(&net, &[A, B]);
        net.feed(node, session(LockState::Unlocked));
        assert_silence(&net, &[A, B]);
        assert!(net.captured.get(&(A, B)).is_none_or(Vec::is_empty));
        net.project(A, B, W2, PID);
        assert_sound(&net, A, B);
    }
    for node in [A, B] {
        let mut net = Net::new(&[A, B], Setup::default());
        net.project(A, B, W1, PID);
        let first = net.feed(node, Input::Command(Command::Panic));
        assert_eq!(audio_closes(&first).len(), 1, "{node:?}: {first:?}");
        assert_silence(&net, &[A, B]);
        // Re-arming does not bring the projection back, so neither the sound.
        net.feed(node, Input::Command(Command::Rearm));
        assert_silence(&net, &[A, B]);
        net.project(A, B, W2, PID);
        assert_sound(&net, A, B);
    }
}
