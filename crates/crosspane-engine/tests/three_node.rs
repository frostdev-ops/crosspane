//! N0: public-engine mesh scenarios. No platform, socket, timer thread or native input runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use crosspane_engine::io::{ClipBytes, Warp};
use crosspane_engine::{
    Command, Engine, EngineConfig, InjectCmd, InjectId, Input, Notice, Output, ProjectionKey,
    ProxyEvent,
};
use crosspane_input::Held;
use crosspane_input::arrange;
use crosspane_input::journal::{Journal, JournalError, MemoryJournal};
use crosspane_input::layout::{Layout, Placed};
use crosspane_platform::{
    AudioEvent, CaptureEvent, CaptureId, CaptureStart, ClipKinds, ClipboardEvent, ClipboardHost,
    EndReason, LocalPasteId, LockState, MotionKind, OverlayEvent, Parked, ParkingKind, PortalId,
    SessionEvent, SessionState, StreamId, WindowEvent, WindowInfo, WindowRole, WindowState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::InputMessage;
use crosspane_protocol::msg::{Capability, ClipFailure, ControlMessage, Placement, Refusal};
use crosspane_protocol::projection::{ProjectionEndReason, ProjectionMessage};
use crosspane_testkit::FakeClipboardHost;
use crosspane_types::ClipKind;
use crosspane_types::audio::AudioKind;
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelRect, PixelSize, PointDevice, PointLogical, PointMm, RectLogical,
    SizeLogical, SizeMm,
};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::GlobalDisplayId;
use crosspane_types::id::{DisplayId, NodeId, ProjectionId, WindowId};
use crosspane_types::input::LockKeys;
use crosspane_types::time::MonoTime;

const A: NodeId = NodeId([1; 32]);
const B: NodeId = NodeId([2; 32]);
const C: NodeId = NodeId([3; 32]);
const NODES: [NodeId; 3] = [A, B, C];
const DISPLAY: DisplayId = DisplayId(1);
const TWIN: DisplayId = DisplayId(7);
const WINDOW: WindowId = WindowId(10);
const KEY: HidUsage = HidUsage::keyboard(4);
const GRANTS: [Capability; 5] = [
    Capability::InputAccept,
    Capability::WindowShare,
    Capability::WindowPresent,
    Capability::WindowBrowse,
    Capability::AudioSpeaker,
];

fn ms(n: u64) -> MonoTime {
    MonoTime::from_nanos(n * 1_000_000)
}
fn key(source: NodeId, projection: u64) -> ProjectionKey {
    ProjectionKey {
        source,
        projection: ProjectionId(projection),
    }
}
fn display(height_mm: f64) -> DisplayInfo {
    DisplayInfo {
        id: DISPLAY,
        name: "N0 fake display".into(),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(100.0, height_mm),
            pixel_size: PixelSize::new(1000, (height_mm * 10.0) as u32),
            scale: 1.0,
            logical_origin: PointLogical::zero(),
        },
        refresh_millihz: 60_000,
        color_space: ColorSpace::Srgb,
        hdr: false,
    }
}
fn window(id: WindowId) -> WindowInfo {
    WindowInfo {
        id,
        title: format!("N0 window {}", id.0),
        app_id: "N0 fake app".into(),
        pid: None,
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
fn projection(peer: NodeId, msg: ProjectionMessage) -> Input {
    control(peer, ControlMessage::Projection(msg))
}
fn grants(peers: &[NodeId]) -> Input {
    Input::Grants(peers.iter().map(|p| (*p, BTreeSet::from(GRANTS))).collect())
}

/// Route the engine's actual peer-addressed outputs through three independent Engine instances.
/// Platform completions are successful, deterministic fakes. Logs retain both sender and output;
/// assertions check observable destinations, releases and callbacks, not private state.
struct Mesh {
    engines: BTreeMap<NodeId, Engine>,
    queue: VecDeque<(NodeId, Input)>,
    log: Vec<(NodeId, Output)>,
    portals: BTreeMap<NodeId, Vec<crosspane_platform::CapturePortal>>,
    portal_targets: BTreeMap<(NodeId, NodeId), PortalId>,
    capture: BTreeMap<NodeId, CaptureId>,
    blocked: BTreeSet<(NodeId, NodeId)>,
    stream: u64,
    now: u64,
    held: BTreeSet<(NodeId, HidUsage)>,
    buttons: BTreeSet<(NodeId, MouseButton)>,
    clipboard: BTreeMap<NodeId, FakeClipboardHost>,
    delayed_clip_reads: Vec<(NodeId, Input)>,
    delay_clip_reads: bool,
    defer_targeting: bool,
    defer_presses: bool,
    fail_next_press: bool,
    defer_releases: bool,
}

#[derive(Default)]
struct RecordedState {
    held: BTreeSet<Held>,
    fail_down: bool,
    fail_up: bool,
    fail_read: bool,
}

#[derive(Clone, Default)]
struct RecordedJournal(Arc<Mutex<RecordedState>>);

impl Journal for RecordedJournal {
    fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
        let mut state = self.0.lock().unwrap();
        state.held.insert(item); // Exercise a write that applied before reporting failure.
        if state.fail_down {
            Err(std::io::Error::other("N1e fake torn down").into())
        } else {
            Ok(())
        }
    }

    fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
        let mut state = self.0.lock().unwrap();
        if state.fail_up {
            return Err(std::io::Error::other("N1e fake up failure").into());
        }
        state.held.remove(&item);
        Ok(())
    }

    fn held(&self) -> Result<Vec<Held>, JournalError> {
        let state = self.0.lock().unwrap();
        if state.fail_read {
            return Err(std::io::Error::other("N1e fake read failure").into());
        }
        Ok(state.held.iter().copied().collect())
    }
}

impl Mesh {
    fn new(split: bool) -> Self {
        Self::with_journals(split, None)
    }

    fn with_journals(split: bool, journals: Option<(RecordedJournal, RecordedJournal)>) -> Self {
        Self::with_startup_journals(split, journals, false)
    }

    fn with_startup_journals(
        split: bool,
        journals: Option<(RecordedJournal, RecordedJournal)>,
        defer_startup: bool,
    ) -> Self {
        let mut startup = Vec::new();
        let engines = NODES
            .into_iter()
            .map(|node| {
                let mut config = EngineConfig::new(node);
                config.accel.base_mm_per_unit = 0.1;
                config.accel.max_gain = 1.0;
                let (e1, e2): (Box<dyn Journal>, Box<dyn Journal>) = if node == A {
                    journals.as_ref().map_or_else(
                        || {
                            (
                                Box::<MemoryJournal>::default() as Box<dyn Journal>,
                                Box::<MemoryJournal>::default() as Box<dyn Journal>,
                            )
                        },
                        |(e1, e2)| {
                            (
                                Box::new(e1.clone()) as Box<dyn Journal>,
                                Box::new(e2.clone()) as Box<dyn Journal>,
                            )
                        },
                    )
                } else {
                    (
                        Box::<MemoryJournal>::default(),
                        Box::<MemoryJournal>::default(),
                    )
                };
                let (engine, initial) = Engine::new(config, e1, e2, ms(0)).unwrap();
                startup.extend(initial.into_iter().map(|o| (node, o)));
                (node, engine)
            })
            .collect();
        let mut m = Self {
            engines,
            queue: VecDeque::new(),
            log: Vec::new(),
            portals: BTreeMap::new(),
            portal_targets: BTreeMap::new(),
            capture: BTreeMap::new(),
            blocked: BTreeSet::new(),
            stream: 0,
            now: 0,
            held: BTreeSet::new(),
            buttons: BTreeSet::new(),
            clipboard: BTreeMap::new(),
            delayed_clip_reads: Vec::new(),
            delay_clip_reads: false,
            defer_targeting: false,
            defer_presses: false,
            fail_next_press: false,
            defer_releases: defer_startup,
        };
        for (node, output) in startup {
            m.complete(node, &output);
            m.log.push((node, output));
        }
        let heights = if split {
            [100.0, 50.0, 50.0]
        } else {
            [100.0; 3]
        };
        let origins = if split {
            [(0.0, 0.0), (100.0, 0.0), (100.0, 50.0)]
        } else {
            [(0.0, 0.0), (100.0, 0.0), (200.0, 0.0)]
        };
        let placements: Vec<_> = NODES
            .into_iter()
            .zip(origins)
            .map(|(node, (x, y))| Placement {
                node,
                display: DISPLAY,
                origin: PointMm::new(x, y),
                version: 1,
            })
            .collect();
        let layout = Layout::new(
            placements
                .iter()
                .zip(heights)
                .map(|(p, height)| Placed {
                    id: GlobalDisplayId {
                        node: p.node,
                        display: p.display,
                    },
                    geometry: display(height).geometry,
                    origin: p.origin,
                })
                .collect(),
            EngineConfig::new(A).layout,
        )
        .unwrap();
        m.portal_targets = layout
            .portals()
            .iter()
            .map(|p| ((p.from.node, p.to.node), p.id))
            .collect();
        for node in NODES {
            m.feed(
                node,
                Input::Session(SessionEvent::State(SessionState {
                    lock: LockState::Unlocked,
                    active: Some(true),
                })),
            );
            for (i, peer) in NODES.into_iter().enumerate() {
                let info = display(heights[i]);
                if peer == node {
                    m.feed(node, Input::LocalDisplays(vec![info]));
                } else {
                    m.feed(
                        node,
                        Input::PeerDisplays {
                            peer,
                            displays: vec![info],
                        },
                    );
                    m.feed(node, Input::PeerUp { peer });
                    m.feed(
                        node,
                        Input::AudioPeer {
                            peer,
                            name: peer.short().to_string(),
                            available: true,
                        },
                    );
                }
            }
            m.feed(
                node,
                grants(&NODES.into_iter().filter(|p| *p != node).collect::<Vec<_>>()),
            );
            m.feed(node, Input::Layout(placements.clone()));
        }
        if !defer_startup {
            m.log.clear();
        }
        m
    }
    fn feed(&mut self, node: NodeId, input: Input) {
        self.queue.push_back((node, input));
        let mut steps = 0;
        while let Some((node, input)) = self.queue.pop_front() {
            steps += 1;
            assert!(steps < 10_000, "fake completion loop");
            let out = self
                .engines
                .get_mut(&node)
                .unwrap()
                .handle(input, ms(self.now));
            for output in out {
                self.complete(node, &output);
                self.log.push((node, output));
            }
        }
    }
    fn answer(&mut self, node: NodeId, input: Input) {
        self.queue.push_back((node, input));
    }
    fn complete(&mut self, node: NodeId, output: &Output) {
        if self.clipboard.contains_key(&node) {
            match output {
                Output::ClipPromise { offer, kinds } => self
                    .clipboard
                    .get_mut(&node)
                    .unwrap()
                    .promise(*offer, *kinds)
                    .unwrap(),
                Output::ClipWithdraw { offer } => self
                    .clipboard
                    .get_mut(&node)
                    .unwrap()
                    .withdraw(*offer)
                    .unwrap(),
                Output::ClipFulfil { paste, data } => self
                    .clipboard
                    .get_mut(&node)
                    .unwrap()
                    .fulfil(*paste, data.as_ref().map(|d| d.0.clone())),
                Output::ClipRead {
                    peer,
                    fetch,
                    kind,
                    max_bytes,
                } => {
                    let result = self
                        .clipboard
                        .get_mut(&node)
                        .unwrap()
                        .read(*kind, *max_bytes)
                        .map(ClipBytes)
                        .map_err(|e| match e {
                            crosspane_platform::PlatformError::Locked => ClipFailure::Locked,
                            crosspane_platform::PlatformError::TooLarge => ClipFailure::TooLarge,
                            _ => ClipFailure::Unavailable,
                        });
                    let input = Input::ClipReadDone {
                        peer: *peer,
                        fetch: *fetch,
                        result,
                    };
                    if self.delay_clip_reads {
                        self.delayed_clip_reads.push((node, input));
                    } else {
                        self.answer(node, input);
                    }
                }
                Output::SendClipData {
                    peer,
                    fetch,
                    kind,
                    data,
                } if !self.blocked.contains(&(node, *peer)) => {
                    self.answer(
                        *peer,
                        Input::ClipData {
                            peer: node,
                            fetch: *fetch,
                            kind: *kind,
                            data: data.clone(),
                        },
                    );
                }
                _ => {}
            }
        }
        match output {
            Output::Inject {
                cmd: InjectCmd::Key { usage, down },
                ..
            } => {
                if *down {
                    // Native key-down repeats are legal; an up nevertheless clears the bit.
                    self.held.insert((node, *usage));
                } else {
                    self.held.remove(&(node, *usage));
                }
            }
            Output::Inject {
                cmd: InjectCmd::ReleaseAll,
                ..
            } => {
                self.held.retain(|(n, _)| *n != node);
                self.buttons.retain(|(n, _)| *n != node);
            }
            Output::Inject {
                cmd: InjectCmd::Button { button, down },
                ..
            } => {
                if *down {
                    self.buttons.insert((node, *button));
                } else {
                    self.buttons.remove(&(node, *button));
                }
            }
            _ => {}
        }
        let response = match output {
            Output::SendControl { peer, msg } if !self.blocked.contains(&(node, *peer)) => {
                Some((*peer, control(node, msg.clone())))
            }
            Output::SendInput { peer, msg } if !self.blocked.contains(&(node, *peer)) => Some((
                *peer,
                Input::Link(LinkEvent::Input {
                    peer: node,
                    msg: msg.clone(),
                }),
            )),
            Output::SendMotion { peer, msg } if !self.blocked.contains(&(node, *peer)) => Some((
                *peer,
                Input::Link(LinkEvent::Motion {
                    peer: node,
                    msg: *msg,
                }),
            )),
            Output::SetPortals(portals) => {
                self.portals.insert(node, portals.clone());
                Some((
                    node,
                    Input::PortalsSet {
                        ids: portals.iter().map(|p| p.id).collect(),
                        result: Ok(()),
                    },
                ))
            }
            Output::ShowOverlay { id, .. } => {
                Some((node, Input::Overlay(OverlayEvent::Visible(*id))))
            }
            Output::BeginCapture { id, .. } => {
                self.capture.insert(node, *id);
                self.answer(node, Input::Capture(CaptureEvent::Started { id: *id }));
                Some((
                    node,
                    Input::CaptureBegun {
                        id: *id,
                        result: Ok(CaptureStart {
                            held_keys: vec![],
                            lock_keys: LockKeys::default(),
                        }),
                    },
                ))
            }
            Output::EndCapture { .. } => self.capture.remove(&node).map(|id| {
                (
                    node,
                    Input::Capture(CaptureEvent::Ended {
                        id,
                        reason: EndReason::Requested,
                    }),
                )
            }),
            Output::Inject {
                cmd: InjectCmd::MoveTo { .. },
                ..
            } if self.defer_targeting => None,
            Output::Inject {
                cmd: InjectCmd::Key { down: true, .. } | InjectCmd::Button { down: true, .. },
                ..
            } if self.defer_presses => None,
            Output::Inject {
                id,
                cmd: InjectCmd::Key { down: true, .. } | InjectCmd::Button { down: true, .. },
            } if self.fail_next_press => {
                // The fake already applied the down above: failure does not mean rejected input.
                self.fail_next_press = false;
                Some((node, Input::InjectDone { id: *id, ok: false }))
            }
            Output::Inject {
                cmd:
                    InjectCmd::Key { down: false, .. }
                    | InjectCmd::Button { down: false, .. }
                    | InjectCmd::Recover { .. },
                ..
            } if self.defer_releases => None,
            Output::Inject { id, .. } => Some((node, Input::InjectDone { id: *id, ok: true })),
            Output::OpenProxy { key, size, .. } => Some((
                node,
                Input::ProxyOpened {
                    key: *key,
                    result: Ok((*size, 1.0)),
                },
            )),
            Output::Park { window, size, .. } | Output::ResizeParked { window, size, .. } => {
                Some((
                    node,
                    Input::Parked {
                        window: *window,
                        result: Ok(Parked {
                            fullscreen: false,
                            window: *window,
                            kind: ParkingKind::Twin,
                            display: TWIN,
                            content: PixelRect::new(
                                crosspane_types::geom::euclid::Point2D::new(50, 40),
                                crosspane_types::geom::euclid::Point2D::new(
                                    50 + size.width as i32,
                                    40 + size.height as i32,
                                ),
                            ),
                        }),
                    },
                ))
            }
            Output::StartCapture { projection, .. } => {
                self.stream += 1;
                Some((
                    node,
                    Input::CaptureStarted {
                        projection: *projection,
                        result: Ok(StreamId(self.stream)),
                    },
                ))
            }
            Output::ActivateWindow { window } => {
                Some((node, Input::Windows(WindowEvent::Focused(Some(*window)))))
            }
            Output::HomeBind { op, install } => Some((
                node,
                Input::HomeBindSet {
                    op: *op,
                    install: *install,
                    result: Ok(()),
                },
            )),
            Output::ReleaseAndWarp { op, .. } => {
                if let Some(id) = self.capture.remove(&node) {
                    self.answer(
                        node,
                        Input::Capture(CaptureEvent::Ended {
                            id,
                            reason: EndReason::Requested,
                        }),
                    );
                }
                Some((
                    node,
                    Input::CaptureReleased {
                        op: *op,
                        result: Ok(Warp::Done),
                    },
                ))
            }
            Output::OpenAudioPlayback { key } => Some((
                node,
                Input::AudioDeviceOpened {
                    key: *key,
                    kind: AudioKind::Speaker,
                    result: Ok(()),
                },
            )),
            _ => None,
        };
        if let Some((node, input)) = response {
            self.answer(node, input);
        }
    }
    fn enable_clipboard(&mut self) {
        for node in NODES {
            self.clipboard.insert(node, FakeClipboardHost::default());
            let Input::Grants(mut grants) =
                grants(&NODES.into_iter().filter(|p| *p != node).collect::<Vec<_>>())
            else {
                unreachable!()
            };
            for caps in grants.values_mut() {
                caps.extend([Capability::ClipboardRead, Capability::ClipboardWrite]);
            }
            self.feed(node, Input::Grants(grants));
            for peer in NODES.into_iter().filter(|p| *p != node) {
                self.feed(
                    node,
                    Input::ClipPeer {
                        peer,
                        available: true,
                    },
                );
            }
        }
    }
    fn clipboard_copy(&mut self, node: NodeId, text: &[u8]) {
        self.clipboard
            .get_mut(&node)
            .unwrap()
            .copy(Some(text.to_vec()), None);
        self.feed(
            node,
            Input::Clipboard(ClipboardEvent::Changed {
                kinds: ClipKinds {
                    text: true,
                    image: false,
                },
            }),
        );
    }
    fn clipboard_paste(&mut self, node: NodeId, paste: u64) {
        let host = self.clipboard.get_mut(&node).unwrap();
        let offer = host.current_promise().unwrap().0;
        host.paste(LocalPasteId(paste), ClipKind::Text);
        self.feed(
            node,
            Input::Clipboard(ClipboardEvent::PasteRequested {
                paste: LocalPasteId(paste),
                offer,
                kind: ClipKind::Text,
            }),
        );
    }
    fn clipboard_focus(&mut self, node: NodeId, key: ProjectionKey, focused: bool) {
        self.feed(
            node,
            Input::Proxy {
                key,
                event: ProxyEvent::Focus(focused),
            },
        );
    }
    fn enter(&mut self, node: NodeId, toward: NodeId, position: f64) {
        // Physical-strip ordering is exposed by installed portals, not manufactured ids.
        let portal = self.portal_targets[&(node, toward)];
        assert!(self.portals[&node].iter().any(|p| p.id == portal));
        self.feed(
            node,
            Input::Capture(CaptureEvent::EdgePressed {
                portal,
                position,
                at: ms(self.now),
            }),
        );
        assert_eq!(self.engines[&node].control_established(), Some(toward));
    }
    fn motion(&mut self, node: NodeId, dx: f64, dy: f64) {
        self.now += 1;
        self.feed(
            node,
            Input::Capture(CaptureEvent::Motion {
                dx,
                dy,
                kind: MotionKind::Unaccelerated,
                at: ms(self.now),
            }),
        );
    }
    fn press(&mut self, node: NodeId, usage: HidUsage, down: bool) {
        self.feed(
            node,
            Input::Capture(CaptureEvent::Key {
                usage,
                down,
                at: ms(self.now),
            }),
        );
    }
    fn e1_item(&mut self, node: NodeId, item: Held, down: bool) {
        let event = match item {
            Held::Key(usage) => CaptureEvent::Key {
                usage,
                down,
                at: ms(self.now),
            },
            Held::Button(button) => CaptureEvent::Button {
                button,
                down,
                at: ms(self.now),
            },
        };
        self.feed(node, Input::Capture(event));
    }
    fn project(&mut self, source: NodeId, destination: NodeId, window: WindowId) -> ProjectionKey {
        self.feed(
            source,
            Input::Windows(WindowEvent::Added(self::window(window))),
        );
        let offset = self.log.len();
        self.feed(
            source,
            Input::Command(Command::Project {
                window,
                to: destination,
                place: None,
            }),
        );
        self.log[offset..]
            .iter()
            .find_map(|(n, o)| match o {
                Output::Notice(Notice::ProjectionStarted { key, peer, .. })
                    if *n == source && *peer == destination =>
                {
                    Some(*key)
                }
                _ => None,
            })
            .expect("live source projection")
    }
    fn count(&self, node: NodeId, predicate: impl Fn(&Output) -> bool) -> usize {
        self.log
            .iter()
            .filter(|(n, o)| *n == node && predicate(o))
            .count()
    }
    fn physical_trace(&self, node: NodeId, item: Held) -> Vec<bool> {
        self.log
            .iter()
            .filter_map(|(n, o)| {
                if *n != node {
                    return None;
                }
                match (item, o) {
                    (
                        Held::Key(wanted),
                        Output::Inject {
                            cmd: InjectCmd::Key { usage, down },
                            ..
                        },
                    ) if wanted == *usage => Some(*down),
                    (
                        Held::Button(wanted),
                        Output::Inject {
                            cmd: InjectCmd::Button { button, down },
                            ..
                        },
                    ) if wanted == *button => Some(*down),
                    (
                        _,
                        Output::Inject {
                            cmd: InjectCmd::ReleaseAll,
                            ..
                        },
                    ) => Some(false),
                    (
                        Held::Key(wanted),
                        Output::Inject {
                            cmd: InjectCmd::Recover { keys, .. },
                            ..
                        },
                    ) if keys.contains(&wanted) => Some(false),
                    (
                        Held::Button(wanted),
                        Output::Inject {
                            cmd: InjectCmd::Recover { buttons, .. },
                            ..
                        },
                    ) if buttons.contains(&wanted) => Some(false),
                    _ => None,
                }
            })
            .collect()
    }
    fn returned(&self, destination: NodeId, key: ProjectionKey, window: WindowId) {
        assert_eq!(
            self.count(
                destination,
                |o| matches!(o, Output::CloseProxy { key: k } if *k == key)
            ),
            1
        );
        assert_eq!(
            self.count(
                key.source,
                |o| matches!(o, Output::Restore { window: w, .. } if *w == window)
            ),
            1
        );
        assert_eq!(self.count(key.source, |o| matches!(o, Output::Notice(Notice::ProjectionEnded { key: k, reason: ProjectionEndReason::Returned }) if *k == key)), 1);
    }
    fn proxy_key(&mut self, node: NodeId, key: ProjectionKey, usage: HidUsage, down: bool) {
        self.feed(
            node,
            Input::Proxy {
                key,
                event: ProxyEvent::Focus(true),
            },
        );
        self.feed(
            node,
            Input::Proxy {
                key,
                event: ProxyEvent::Key { usage, down },
            },
        );
    }

    fn proxy_item(&mut self, node: NodeId, key: ProjectionKey, item: Held, down: bool) {
        match item {
            Held::Key(usage) => self.proxy_key(node, key, usage, down),
            Held::Button(button) => {
                self.feed(
                    node,
                    Input::Proxy {
                        key,
                        event: ProxyEvent::Focus(true),
                    },
                );
                self.feed(
                    node,
                    Input::Proxy {
                        key,
                        event: ProxyEvent::Button {
                            button,
                            down,
                            position: PointDevice::new(50.0, 50.0),
                        },
                    },
                );
            }
        }
    }

    fn heartbeat(&mut self, peer: NodeId, key: ProjectionKey, item: Held, seq: u32) {
        let (keys, buttons) = match item {
            Held::Key(usage) => (vec![usage], vec![]),
            Held::Button(button) => (vec![], vec![button]),
        };
        self.feed(
            key.source,
            Input::Link(LinkEvent::Input {
                peer,
                msg: InputMessage::Proj(crosspane_protocol::projection::ProjInput::Held {
                    projection: key.projection,
                    seq,
                    keys,
                    buttons,
                }),
            }),
        );
    }
}

#[test]
fn chain_switch_and_two_hop_return_keep_one_capture_and_release_old_target() {
    let mut m = Mesh::new(false);
    m.enter(A, B, 0.5);
    m.press(A, KEY, true);
    m.motion(A, 2000.0, 0.0);
    assert_eq!(m.engines[&A].control_established(), Some(C));
    assert_eq!(m.engines[&B].controlled_by(), None);
    assert_eq!(m.engines[&C].controlled_by(), Some(A));
    assert_eq!(
        m.count(B, |o| matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::Key {
                    usage: KEY,
                    down: false
                },
                ..
            }
        )),
        1
    );
    m.press(A, KEY, false);
    assert_eq!(
        m.count(C, |o| matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::Key { usage: KEY, .. },
                ..
            }
        )),
        0,
        "old held keys are not replayed at C"
    );
    m.motion(A, 50.0, 0.0); // clear C's disarmed entry edge
    m.motion(A, -2000.0, 0.0);
    assert_eq!(m.engines[&A].control_established(), Some(B));
    m.motion(A, -50.0, 0.0); // clear B's disarmed entry edge
    m.motion(A, -2000.0, 0.0);
    assert_eq!(m.engines[&A].controlling(), None);
    assert!(
        NODES
            .into_iter()
            .all(|n| m.engines[&n].controlled_by().is_none())
    );
    assert_eq!(m.count(A, |o| matches!(o, Output::BeginCapture { .. })), 1);
    assert_eq!(m.count(A, |o| matches!(o, Output::EndCapture { .. })), 1);
}

#[test]
fn a_controlled_b_has_one_seat_and_cannot_start_its_own_control_chain() {
    let mut m = Mesh::new(false);
    let b_to_c = m.portals[&B].last().unwrap().id;
    m.enter(A, B, 0.5);
    m.log.clear();
    m.feed(
        B,
        Input::Capture(CaptureEvent::EdgePressed {
            portal: b_to_c,
            position: 0.5,
            at: ms(0),
        }),
    );
    assert_eq!(m.engines[&B].controlling(), None);
    assert_eq!(
        m.count(B, |o| matches!(
            o,
            Output::SendControl {
                msg: ControlMessage::StartControl { .. },
                ..
            }
        )),
        0
    );
    assert_eq!(m.engines[&B].controlled_by(), Some(A));
    // C's physical seat may request B, but cannot steal the seat A still owns.
    let portal = m.portal_targets[&(C, B)];
    m.feed(
        C,
        Input::Capture(CaptureEvent::EdgePressed {
            portal,
            position: 0.5,
            at: ms(0),
        }),
    );
    assert_eq!(m.engines[&C].controlling(), None);
    assert_eq!(m.engines[&B].controlled_by(), Some(A));
    assert_eq!(
        m.count(C, |o| matches!(
            o,
            Output::Notice(Notice::Refused {
                peer: B,
                reason: Refusal::Busy
            })
        )),
        1
    );
}

#[test]
fn split_edge_selects_both_peers_without_portal_aliasing() {
    let mut m = Mesh::new(true);
    assert_eq!(m.portals[&A].len(), 2);
    assert_ne!(m.portals[&A][0].id, m.portals[&A][1].id);
    m.enter(A, B, 0.25);
    m.feed(A, Input::Command(Command::ReleaseControl));
    m.feed(A, Input::Command(Command::Rearm));
    m.enter(A, C, 0.75);
    assert_eq!(m.engines[&C].controlled_by(), Some(A));
    assert_eq!(m.engines[&B].controlled_by(), None);
}

#[test]
fn release_chord_after_switch_releases_c_without_touching_b_again() {
    let mut m = Mesh::new(false);
    m.enter(A, B, 0.5);
    m.motion(A, 2000.0, 0.0);
    m.log.clear();
    for usage in [0xe0, 0xe1, 0xe2, 0x29] {
        m.press(A, HidUsage::keyboard(usage), true);
    }
    assert_eq!(m.engines[&A].controlling(), None);
    assert_eq!(m.engines[&C].controlled_by(), None);
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::Notice(Notice::ControlReleased { peer: C, .. })
        )),
        1
    );
    assert_eq!(
        m.count(B, |o| matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::Key { .. },
                ..
            }
        )),
        0
    );
    assert_eq!(m.count(C, |o| matches!(o, Output::Inject { cmd: InjectCmd::Key { usage, down: false }, .. } if [0xe0,0xe1,0xe2].contains(&usage.id))), 3);
}

#[test]
fn c_lease_expiry_after_switch_releases_only_c() {
    let mut m = Mesh::new(false);
    m.enter(A, B, 0.5);
    m.motion(A, 2000.0, 0.0);
    m.press(A, KEY, true);
    m.log.clear();
    m.now = 400;
    m.feed(C, Input::Tick); // no fabricated heartbeat from the disconnected controller
    assert_eq!(
        m.count(C, |o| matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::Key {
                    usage: KEY,
                    down: false
                },
                ..
            }
        )),
        1
    );
    assert_eq!(m.count(B, |o| matches!(o, Output::Inject { .. })), 0);
}

#[test]
fn two_sources_with_projection_one_and_window_ten_share_b_without_aliasing() {
    let mut m = Mesh::new(false);
    let a = m.project(A, B, WINDOW);
    let c = m.project(C, B, WINDOW);
    assert_eq!(a, key(A, 1));
    assert_eq!(c, key(C, 1));
    m.log.clear();
    m.feed(B, Input::Command(Command::Return(a)));
    assert_eq!(
        m.count(B, |o| matches!(o, Output::CloseProxy { key } if *key == a)),
        1
    );
    assert_eq!(
        m.count(B, |o| matches!(o, Output::CloseProxy { key } if *key == c)),
        0
    );
    m.proxy_key(B, c, KEY, true);
    assert_eq!(
        m.count(C, |o| matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::Key {
                    usage: KEY,
                    down: true
                },
                ..
            }
        )),
        1
    );
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::Key { .. },
                ..
            }
        )),
        0
    );
}

#[test]
fn one_source_two_windows_and_wrong_peer_close_keep_ownership() {
    let mut m = Mesh::new(false);
    let b = m.project(A, B, WINDOW);
    let c = m.project(A, C, WindowId(11));
    assert_eq!(b, key(A, 1));
    assert_eq!(c, key(A, 2));
    m.log.clear();
    m.feed(
        A,
        projection(
            B,
            ProjectionMessage::Close {
                projection: c.projection,
                reason: ProjectionEndReason::Returned,
            },
        ),
    );
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::Restore { .. } | Output::StopCapture { .. }
        )),
        0
    );
    m.feed(C, Input::Command(Command::Return(c)));
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::Restore {
                window: WindowId(11),
                ..
            }
        )),
        1
    );
    assert_eq!(m.count(B, |o| matches!(o, Output::CloseProxy { .. })), 0);
    m.proxy_key(B, b, KEY, true);
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::Key {
                    usage: KEY,
                    down: true
                },
                ..
            }
        )),
        1
    );
}

#[test]
fn browse_and_pull_two_sources_with_same_request_and_window_ids() {
    let mut m = Mesh::new(false);
    for node in [A, C] {
        m.feed(node, Input::Windows(WindowEvent::Added(window(WINDOW))));
    }
    for peer in [A, C] {
        m.feed(B, Input::Command(Command::Browse { peer, request: 77 }));
    }
    for peer in [A, C] {
        assert_eq!(m.count(B, |o| matches!(o, Output::BrowseResult { peer: p, request: 77, result: Ok(w) } if *p == peer && w.len() == 1 && w[0].window == WINDOW)), 1);
        m.feed(
            B,
            Input::Command(Command::Pull {
                peer,
                window: WINDOW,
                request: 78,
            }),
        );
    }
    for source in [A, C] {
        assert_eq!(
            m.count(
                B,
                |o| matches!(o, Output::OpenProxy { key: k, .. } if *k == key(source, 1))
            ),
            1
        );
    }
}

#[test]
fn missing_b_grant_does_not_deny_c_and_revoke_a_ends_on_b_and_c() {
    let mut m = Mesh::new(false);
    m.feed(A, grants(&[C]));
    m.feed(A, Input::Windows(WindowEvent::Added(window(WINDOW))));
    m.feed(
        A,
        Input::Command(Command::Project {
            window: WINDOW,
            to: B,
            place: None,
        }),
    );
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::Notice(Notice::ProjectionRefused {
                peer: B,
                reason: Refusal::Permission
            })
        )),
        1
    );
    let c = m.project(A, C, WINDOW);
    m.feed(A, grants(&[B, C]));
    let b = m.project(A, B, WindowId(11));
    m.log.clear();
    // This is the engine-visible result of signed revocation on each surviving node.
    // Signature authentication and agent notice fan-out are explicitly static-audited separately.
    m.feed(B, grants(&[C]));
    m.feed(C, grants(&[B]));
    for (node, k) in [(B, b), (C, c)] {
        assert_eq!(
            m.count(
                node,
                |o| matches!(o, Output::CloseProxy { key } if *key == k)
            ),
            1
        );
        assert_eq!(m.count(A, |o| matches!(o, Output::Notice(Notice::ProjectionEnded { key, reason: ProjectionEndReason::Revoked }) if *key == k)), 1);
    }
}

#[test]
fn revoking_c_preserves_a_projection_and_control_on_b() {
    let mut m = Mesh::new(false);
    let a = m.project(A, B, WINDOW);
    let c = m.project(C, B, WINDOW);
    m.enter(A, B, 0.5);
    m.log.clear();
    m.feed(B, grants(&[A]));
    assert_eq!(m.engines[&B].controlled_by(), Some(A));
    assert_eq!(
        m.count(B, |o| matches!(o, Output::CloseProxy { key } if *key == c)),
        1
    );
    assert_eq!(
        m.count(B, |o| matches!(o, Output::CloseProxy { key } if *key == a)),
        0
    );
}

#[test]
fn revoking_a_input_grant_ends_control_on_each_other_node() {
    for target in [B, C] {
        // Each grant change begins with a fresh physical gesture and controller capture.
        let mut m = Mesh::new(true);
        m.enter(A, target, if target == B { 0.25 } else { 0.75 });
        m.press(A, KEY, true);
        m.log.clear();
        m.feed(target, grants(&[if target == B { C } else { B }]));
        assert_eq!(m.engines[&target].controlled_by(), None);
        assert_eq!(m.engines[&A].controlling(), None);
        assert_eq!(
            m.count(target, |o| matches!(
                o,
                Output::Inject {
                    cmd: InjectCmd::Key {
                        usage: KEY,
                        down: false
                    },
                    ..
                }
            )),
            1,
            "target {target:?}: {:?}",
            m.log
        );
        assert!(!m.held.contains(&(target, KEY)));
        assert_eq!(
            m.count(if target == B { C } else { B }, |o| matches!(
                o,
                Output::Inject { .. }
            )),
            0
        );
        m.press(A, KEY, false);
        m.feed(A, Input::Command(Command::Rearm));
    }
}

#[test]
fn one_peer_link_loss_does_not_end_the_other_projection() {
    let mut m = Mesh::new(false);
    let b = m.project(A, B, WINDOW);
    let c = m.project(A, C, WindowId(11));
    m.blocked.extend([(A, B), (B, A)]);
    m.feed(
        A,
        Input::Link(LinkEvent::Closed {
            peer: B,
            error: LinkError::Closed,
        }),
    );
    m.feed(
        B,
        Input::Link(LinkEvent::Closed {
            peer: A,
            error: LinkError::Closed,
        }),
    );
    m.log.clear();
    m.now = 20_001;
    for node in NODES {
        m.feed(node, Input::Tick);
    }
    assert_eq!(m.count(A, |o| matches!(o, Output::Notice(Notice::ProjectionEnded { key, reason: ProjectionEndReason::LinkLost }) if *key == b)), 1);
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::Restore {
                window: WindowId(11),
                ..
            }
        )),
        0
    );
    assert_eq!(
        m.count(C, |o| matches!(o, Output::CloseProxy { key } if *key == c)),
        0
    );
    m.proxy_key(C, c, KEY, true);
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::Key {
                    usage: KEY,
                    down: true
                },
                ..
            }
        )),
        1
    );
}

#[test]
fn home_on_twin_with_third_peer_keeps_other_projection_live_and_filters_input() {
    let mut m = Mesh::new(false);
    let b = m.project(A, B, WINDOW);
    let c = m.project(A, C, WindowId(11));
    m.feed(
        B,
        Input::Proxy {
            key: b,
            event: ProxyEvent::Placed {
                display: Some(DISPLAY),
                origin: PointDevice::new(200.0, 300.0),
                size: PixelSize::new(400, 300),
            },
        },
    );
    m.enter(A, B, 0.5);
    m.motion(A, 250.0, -100.0);
    m.feed(
        B,
        Input::Proxy {
            key: b,
            event: ProxyEvent::Motion {
                position: PointDevice::new(50.0, 100.0),
            },
        },
    );
    assert_eq!(
        m.count(
            A,
            |o| matches!(o, Output::Notice(Notice::Home { key, entered: true }) if *key == b)
        ),
        1
    );
    m.log.clear();
    m.proxy_key(C, c, KEY, true);
    assert_eq!(m.count(C, |o| matches!(o, Output::SendInput { peer: A, msg: InputMessage::Proj(crosspane_protocol::projection::ProjInput::Key { projection, usage: KEY, down: true, .. }) } if *projection == c.projection)), 1, "positive control: C actually sends the key into the fence");
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::Key { usage: KEY, .. },
                ..
            }
        )),
        0,
        "third-peer proxy input must not enter the home bind"
    );
    assert_eq!(
        m.count(C, |o| matches!(o, Output::CloseProxy { key } if *key == c)),
        0,
        "home fences input, not the independent video projection"
    );
    m.feed(A, Input::Command(Command::ReleaseControl));
    assert_eq!(
        m.count(A, |o| matches!(o, Output::HomeBind { install: false, .. })),
        1
    );
    assert_eq!(m.engines[&A].controlling(), None);
}

#[test]
fn two_audio_peers_have_independent_stream_ids_and_grant_teardown() {
    let mut m = Mesh::new(false);
    for peer in [B, C] {
        m.feed(
            A,
            Input::Audio(AudioEvent::VirtualActive {
                peer,
                kind: AudioKind::Speaker,
                active: true,
            }),
        );
    }
    let streams: Vec<_> = m
        .log
        .iter()
        .filter_map(|(node, o)| match o {
            Output::StartAudioStream {
                key,
                kind: AudioKind::Speaker,
                ..
            } if *node == A => Some(*key),
            _ => None,
        })
        .collect();
    assert_eq!(streams.len(), 2);
    assert_eq!(
        streams[0].stream, streams[1].stream,
        "numeric ids are connection-local"
    );
    assert_ne!(streams[0].peer, streams[1].peer);
    m.log.clear();
    m.feed(A, grants(&[B]));
    assert_eq!(
        m.count(
            A,
            |o| matches!(o, Output::StopAudioStream { key } if key.peer == C)
        ),
        1
    );
    assert_eq!(
        m.count(
            A,
            |o| matches!(o, Output::StopAudioStream { key } if key.peer == B)
        ),
        0
    );
    assert_eq!(
        m.count(B, |o| matches!(o, Output::CloseAudioPlayback { .. })),
        0
    );
    assert_eq!(
        m.count(C, |o| matches!(o, Output::CloseAudioPlayback { .. })),
        1
    );
}

#[test]
fn ending_one_projection_preserves_the_other_peers_held_key() {
    let mut m = Mesh::new(false);
    let b = m.project(A, B, WINDOW);
    let c = m.project(A, C, WindowId(11));
    m.proxy_key(B, b, KEY, true);
    m.proxy_key(C, c, KEY, true);
    for (node, key) in [(B, b), (C, c)] {
        assert_eq!(m.count(node, |o| matches!(o, Output::SendInput { peer: A, msg: InputMessage::Proj(crosspane_protocol::projection::ProjInput::Key { projection, usage: KEY, down: true, .. }) } if *projection == key.projection)), 1);
    }
    assert!(m.held.contains(&(A, KEY)));
    // A correct physical union may coalesce the second down; native key repeats are also legal.
    m.log.clear();
    m.feed(B, Input::Command(Command::Return(b)));
    m.returned(B, b, WINDOW);
    let b_return_trace = m.physical_trace(A, Held::Key(KEY));
    let c_still_physically_held = m.held.contains(&(A, KEY));
    assert_eq!(m.count(C, |o| matches!(o, Output::CloseProxy { .. })), 0);
    m.log.clear();
    m.feed(C, Input::Command(Command::Return(c)));
    m.returned(C, c, WindowId(11));
    let c_return_trace = m.physical_trace(A, Held::Key(KEY));
    assert_eq!(m.count(B, |o| matches!(o, Output::CloseProxy { .. })), 0);
    assert!(!m.held.contains(&(A, KEY)));
    assert_eq!(
        b_return_trace,
        Vec::<bool>::new(),
        "B return must submit no physical key action while C holds it (including up-then-re-press)"
    );
    assert_eq!(
        c_return_trace,
        vec![false],
        "C return must submit exactly one final physical key-up"
    );
    assert!(
        c_still_physically_held,
        "B return sent a physical key-up while C still owned its press"
    );
}

#[test]
fn ending_one_projection_preserves_the_other_peers_held_button() {
    let mut m = Mesh::new(false);
    let b = m.project(A, B, WINDOW);
    let c = m.project(A, C, WindowId(11));
    for (node, key) in [(B, b), (C, c)] {
        m.feed(
            node,
            Input::Proxy {
                key,
                event: ProxyEvent::Focus(true),
            },
        );
        m.feed(
            node,
            Input::Proxy {
                key,
                event: ProxyEvent::Button {
                    button: MouseButton::PRIMARY,
                    down: true,
                    position: PointDevice::new(50.0, 50.0),
                },
            },
        );
    }
    for (node, key) in [(B, b), (C, c)] {
        assert_eq!(m.count(node, |o| matches!(o, Output::SendInput { peer: A, msg: InputMessage::Proj(crosspane_protocol::projection::ProjInput::Button { projection, button: MouseButton::PRIMARY, down: true, .. }) } if *projection == key.projection)), 1);
    }
    assert!(m.buttons.contains(&(A, MouseButton::PRIMARY)));
    m.log.clear();
    m.feed(B, Input::Command(Command::Return(b)));
    m.returned(B, b, WINDOW);
    let b_return_trace = m.physical_trace(A, Held::Button(MouseButton::PRIMARY));
    let c_still_physically_held = m.buttons.contains(&(A, MouseButton::PRIMARY));
    assert_eq!(m.count(C, |o| matches!(o, Output::CloseProxy { .. })), 0);
    m.log.clear();
    m.feed(C, Input::Command(Command::Return(c)));
    m.returned(C, c, WindowId(11));
    let c_return_trace = m.physical_trace(A, Held::Button(MouseButton::PRIMARY));
    assert_eq!(m.count(B, |o| matches!(o, Output::CloseProxy { .. })), 0);
    assert!(!m.buttons.contains(&(A, MouseButton::PRIMARY)));
    assert_eq!(
        b_return_trace,
        Vec::<bool>::new(),
        "B return must submit no physical button action while C holds it (including up-then-re-press)"
    );
    assert_eq!(
        c_return_trace,
        vec![false],
        "C return must submit exactly one final physical primary-up"
    );
    assert!(
        c_still_physically_held,
        "B return sent a physical primary-up while C still owned its press"
    );
}

#[test]
fn three_node_default_layout_is_identical_from_every_local_order() {
    let orders = [[A, B, C], [C, A, B], [B, C, A]];
    let expected = arrange::default_layout(&orders[0].map(|n| (n, vec![display(100.0)])));
    for order in orders {
        assert_eq!(
            arrange::default_layout(&order.map(|n| (n, vec![display(100.0)]))),
            expected
        );
    }
    assert_eq!(
        expected.iter().map(|p| p.origin.x).collect::<Vec<_>>(),
        vec![0.0, 100.0, 200.0]
    );
}

#[test]
fn owner_end_paths_preserve_the_other_peers_key_and_button() {
    for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        for path in ["return", "revoke", "link grace", "lease expiry"] {
            let mut m = Mesh::new(false);
            let b = m.project(A, B, WINDOW);
            let c = m.project(A, C, WindowId(11));
            m.proxy_item(B, b, item, true);
            m.proxy_item(C, c, item, true);
            m.log.clear();
            match path {
                "return" => {
                    m.feed(B, Input::Command(Command::Return(b)));
                    m.returned(B, b, WINDOW);
                }
                "revoke" => {
                    m.feed(A, grants(&[C]));
                    assert_eq!(m.count(A, |o| matches!(o,
                        Output::Notice(Notice::ProjectionEnded { key, reason: ProjectionEndReason::Revoked })
                        if *key == b)), 1);
                }
                "link grace" => {
                    m.blocked.extend([(A, B), (B, A)]);
                    m.feed(
                        A,
                        Input::Link(LinkEvent::Closed {
                            peer: B,
                            error: LinkError::Closed,
                        }),
                    );
                    assert!(
                        m.physical_trace(A, item).is_empty(),
                        "link suspension must retain C"
                    );
                    m.now = 19_999;
                    m.heartbeat(C, c, item, 100);
                    m.now = 20_000;
                    m.feed(A, Input::Tick);
                    assert_eq!(m.count(A, |o| matches!(o,
                        Output::Notice(Notice::ProjectionEnded { key, reason: ProjectionEndReason::LinkLost })
                        if *key == b)), 1);
                }
                "lease expiry" => {
                    m.now = 100;
                    m.heartbeat(C, c, item, 100);
                    m.now = 301;
                    m.feed(A, Input::Tick);
                    assert_eq!(m.count(B, |o| matches!(o, Output::CloseProxy { .. })), 0);
                }
                _ => unreachable!(),
            }
            assert!(
                m.physical_trace(A, item).is_empty(),
                "{path} lifted {item:?}"
            );
            assert_eq!(m.count(C, |o| matches!(o, Output::CloseProxy { .. })), 0);
            m.log.clear();
            m.feed(C, Input::Command(Command::Return(c)));
            m.returned(C, c, WindowId(11));
            assert_eq!(
                m.physical_trace(A, item),
                vec![false],
                "final {path}: {item:?}"
            );
        }
    }
}

#[test]
fn failed_or_timed_out_targeting_preserves_the_other_peers_held_key() {
    for timeout in [false, true] {
        let mut m = Mesh::new(false);
        let b = m.project(A, B, WINDOW);
        let c = m.project(A, C, WindowId(11));
        m.proxy_key(B, b, KEY, true);
        m.proxy_key(C, c, KEY, true);
        m.log.clear();
        m.defer_targeting = true;
        m.proxy_item(B, b, Held::Button(MouseButton::PRIMARY), true);
        let id = m
            .log
            .iter()
            .find_map(|(node, output)| match output {
                Output::Inject {
                    id,
                    cmd: InjectCmd::MoveTo { .. },
                } if *node == A => Some(*id),
                _ => None,
            })
            .expect("actual button targeting request");
        if timeout {
            for t in [100, 200, 300, 400, 499] {
                m.now = t;
                m.heartbeat(B, b, Held::Key(KEY), t as u32 + 100);
                m.heartbeat(C, c, Held::Key(KEY), t as u32 + 100);
            }
            m.now = 500;
            m.feed(A, Input::Tick);
            assert_eq!(
                m.count(A, |o| matches!(o,
                Output::Notice(Notice::ProjectionEnded { key, reason: ProjectionEndReason::Failed })
                if *key == b)),
                1
            );
        } else {
            m.feed(A, Input::InjectDone { id, ok: false });
            // A failed targeting operation discards the unissued click; the live source then returns.
            m.feed(B, Input::Command(Command::Return(b)));
            m.returned(B, b, WINDOW);
        }
        assert!(m.physical_trace(A, Held::Key(KEY)).is_empty());
        assert!(
            m.physical_trace(A, Held::Button(MouseButton::PRIMARY))
                .is_empty()
        );
        assert!(m.held.contains(&(A, KEY)));
        m.feed(A, Input::InjectDone { id, ok: true });
        assert!(
            m.buttons.is_empty(),
            "late targeting completion must not invent a press"
        );
        m.log.clear();
        m.feed(C, Input::Command(Command::Return(c)));
        m.returned(C, c, WindowId(11));
        assert_eq!(m.physical_trace(A, Held::Key(KEY)), vec![false]);
    }
}

#[test]
fn failed_press_cleanup_never_represses_after_focus_changes() {
    let mut m = Mesh::new(false);
    let b = m.project(A, B, WINDOW);
    let c = m.project(A, C, WindowId(11));
    m.fail_next_press = true;
    m.defer_releases = true;
    m.proxy_key(B, b, KEY, true);
    m.feed(A, Input::Windows(WindowEvent::Focused(Some(WindowId(999)))));
    m.log.clear();
    m.now = 51;
    m.feed(A, Input::Tick);
    assert!(
        !m.physical_trace(A, Held::Key(KEY)).contains(&true),
        "cleanup retry must never press into the newly focused local window"
    );
    let cleanup = m.log.iter().find_map(|(node, o)| match o {
        Output::Inject {
            id,
            cmd: InjectCmd::Key { down: false, .. },
        } if *node == A => Some(*id),
        _ => None,
    });
    m.feed(
        A,
        Input::InjectDone {
            id: cleanup.expect("immediate cleanup up"),
            ok: true,
        },
    );
    assert!(!m.held.contains(&(A, KEY)));
    m.log.clear();
    for seq in [100, 101] {
        m.feed(
            A,
            Input::Link(LinkEvent::Input {
                peer: C,
                msg: InputMessage::Proj(crosspane_protocol::projection::ProjInput::Key {
                    projection: c.projection,
                    seq,
                    usage: KEY,
                    down: true,
                }),
            }),
        );
        if seq == 100 {
            assert!(m.physical_trace(A, Held::Key(KEY)).is_empty());
            assert_eq!(
                m.count(
                    A,
                    |o| matches!(o, Output::ActivateWindow { window } if *window == WindowId(11))
                ),
                1
            );
            m.log.clear();
        }
    }
    assert_eq!(m.physical_trace(A, Held::Key(KEY)), vec![true]);
}

#[test]
fn failed_button_cleanup_never_bypasses_pending_pointer_targeting() {
    let mut m = Mesh::new(false);
    let b = m.project(A, B, WINDOW);
    let c = m.project(A, C, WindowId(11));
    m.fail_next_press = true;
    m.defer_releases = true;
    m.proxy_item(B, b, Held::Button(MouseButton::PRIMARY), true);
    m.defer_targeting = true;
    m.log.clear();
    m.proxy_item(C, c, Held::Button(MouseButton::PRIMARY), true);
    let target = m.log.iter().find_map(|(node, o)| match o {
        Output::Inject {
            id,
            cmd: InjectCmd::MoveTo { .. },
        } if *node == A => Some(*id),
        _ => None,
    });
    assert!(target.is_none(), "a blocked down cannot begin MoveTo");
    m.log.clear();
    m.now = 51;
    m.feed(A, Input::Tick);
    assert!(
        !m.physical_trace(A, Held::Button(MouseButton::PRIMARY))
            .contains(&true),
        "cleanup retry must never press around the outstanding MoveTo"
    );
    assert!(
        !m.buttons.contains(&(A, MouseButton::PRIMARY)),
        "a down admitted before cleanup confirmation is absorbed"
    );
    let cleanup = m.log.iter().find_map(|(node, o)| match o {
        Output::Inject {
            id,
            cmd: InjectCmd::Button { down: false, .. },
        } if *node == A => Some(*id),
        _ => None,
    });
    m.feed(
        A,
        Input::InjectDone {
            id: cleanup.expect("immediate cleanup up"),
            ok: true,
        },
    );
    m.proxy_item(C, c, Held::Button(MouseButton::PRIMARY), false);
    m.log.clear();
    m.proxy_item(C, c, Held::Button(MouseButton::PRIMARY), true);
    assert!(
        m.physical_trace(A, Held::Button(MouseButton::PRIMARY))
            .is_empty()
    );
    let fresh_target = m
        .log
        .iter()
        .find_map(|(node, o)| match o {
            Output::Inject {
                id,
                cmd: InjectCmd::MoveTo { .. },
            } if *node == A => Some(*id),
            _ => None,
        })
        .expect("new press needs a new MoveTo");
    m.feed(
        A,
        Input::InjectDone {
            id: fresh_target,
            ok: true,
        },
    );
    assert_eq!(
        m.physical_trace(A, Held::Button(MouseButton::PRIMARY)),
        vec![true]
    );
}

#[test]
fn ending_e1_target_preserves_another_peers_e2_hold() {
    let mut m = Mesh::new(false);
    let c = m.project(A, C, WINDOW);
    m.enter(B, A, 0.5);
    m.press(B, KEY, true);
    assert_eq!(m.engines[&A].controlled_by(), Some(B));
    m.proxy_key(C, c, KEY, true);
    assert!(m.held.contains(&(A, KEY)));
    m.log.clear();
    // Removing only B's grant ends A's real E1 target session; C's projection stays authorized.
    m.feed(A, grants(&[C]));
    assert_eq!(m.engines[&A].controlled_by(), None);
    assert_eq!(
        m.count(A, |o| matches!(o, Output::Notice(Notice::ControlEnded(B)))),
        1
    );
    assert_eq!(m.count(C, |o| matches!(o, Output::CloseProxy { .. })), 0);
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::ReleaseAll,
                ..
            }
        )),
        0,
        "normal E1 teardown must not issue global cleanup"
    );
    assert!(
        m.held.contains(&(A, KEY)),
        "E1 cleanup must preserve C's E2 physical hold"
    );
    assert!(m.physical_trace(A, Held::Key(KEY)).is_empty());
    m.log.clear();
    m.feed(C, Input::Command(Command::Return(c)));
    m.returned(C, c, WINDOW);
    assert_eq!(m.physical_trace(A, Held::Key(KEY)), vec![false]);
    assert!(m.held.is_empty());
}

#[test]
fn normal_e1_e2_acquisition_and_teardown_orders_share_one_physical_hold() {
    for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        for e1_first in [false, true] {
            for end_e1_first in [false, true] {
                let mut m = Mesh::new(false);
                let c = m.project(A, C, WINDOW);
                m.enter(B, A, 0.5);
                m.log.clear();
                if e1_first {
                    m.e1_item(B, item, true);
                }
                m.proxy_item(C, c, item, true);
                if !e1_first {
                    m.e1_item(B, item, true);
                }
                assert_eq!(m.physical_trace(A, item), vec![true]);
                m.log.clear();
                if end_e1_first {
                    m.feed(B, Input::Command(Command::ReleaseControl));
                    assert_eq!(m.engines[&A].controlled_by(), None);
                    assert_eq!(m.count(C, |o| matches!(o, Output::CloseProxy { .. })), 0);
                } else {
                    m.feed(C, Input::Command(Command::Return(c)));
                    m.returned(C, c, WINDOW);
                    assert_eq!(m.engines[&A].controlled_by(), Some(B));
                }
                assert!(m.physical_trace(A, item).is_empty());
                m.log.clear();
                if end_e1_first {
                    m.feed(C, Input::Command(Command::Return(c)));
                    m.returned(C, c, WINDOW);
                } else {
                    m.feed(B, Input::Command(Command::ReleaseControl));
                }
                assert_eq!(m.physical_trace(A, item), vec![false]);
                assert!(m.held.is_empty());
                assert!(m.buttons.is_empty());
                assert_eq!(m.engines[&A].controlled_by(), None);
                assert_eq!(
                    m.count(A, |o| matches!(
                        o,
                        Output::Inject {
                            cmd: InjectCmd::ReleaseAll,
                            ..
                        }
                    )),
                    0
                );
            }
        }
    }
}

#[test]
fn normal_e1_end_releases_only_its_unshared_items() {
    let mut m = Mesh::new(false);
    let c = m.project(A, C, WINDOW);
    m.enter(B, A, 0.5);
    m.e1_item(B, Held::Key(KEY), true);
    m.e1_item(B, Held::Button(MouseButton::PRIMARY), true);
    m.proxy_item(C, c, Held::Key(KEY), true);
    m.log.clear();
    m.feed(B, Input::Command(Command::ReleaseControl));
    assert!(m.physical_trace(A, Held::Key(KEY)).is_empty());
    assert_eq!(
        m.physical_trace(A, Held::Button(MouseButton::PRIMARY)),
        vec![false]
    );
    assert!(m.held.contains(&(A, KEY)));
    assert!(m.buttons.is_empty());
    m.log.clear();
    m.feed(C, Input::Command(Command::Return(c)));
    m.returned(C, c, WINDOW);
    assert_eq!(m.physical_trace(A, Held::Key(KEY)), vec![false]);
    assert!(m.held.is_empty());
}

fn blocked_down_callback_orders(item: Held, cleanup_first: bool) {
    let mut m = Mesh::new(false);
    let b = m.project(A, B, WINDOW);
    let c = m.project(A, C, WindowId(11));
    m.fail_next_press = true;
    m.defer_releases = true;
    m.proxy_item(B, b, item, true);
    let cleanup = m
        .log
        .iter()
        .find_map(|(node, output)| match (item, output) {
            (
                Held::Key(key),
                Output::Inject {
                    id,
                    cmd: InjectCmd::Key { usage, down: false },
                },
            ) if *node == A && *usage == key => Some(*id),
            (
                Held::Button(key),
                Output::Inject {
                    id,
                    cmd:
                        InjectCmd::Button {
                            button,
                            down: false,
                        },
                },
            ) if *node == A && *button == key => Some(*id),
            _ => None,
        })
        .expect("failed establishing press emitted cleanup");
    m.defer_targeting = true;
    m.log.clear();
    if matches!(item, Held::Key(_)) {
        // A different item's MoveTo keeps the real source FIFO busy while the key is blocked.
        m.proxy_item(C, c, Held::Button(MouseButton(2)), true);
    }
    m.proxy_item(C, c, item, true);
    let target = m.log.iter().find_map(|(node, output)| match output {
        Output::Inject {
            id,
            cmd: InjectCmd::MoveTo { .. },
        } if *node == A => Some(*id),
        _ => None,
    });
    if cleanup_first {
        m.feed(
            A,
            Input::InjectDone {
                id: cleanup,
                ok: true,
            },
        );
    }
    if let Some(id) = target {
        m.feed(A, Input::InjectDone { id, ok: true });
    }
    if !cleanup_first {
        m.feed(
            A,
            Input::InjectDone {
                id: cleanup,
                ok: true,
            },
        );
    }
    assert!(
        m.physical_trace(A, item).is_empty(),
        "a down admitted during cleanup cannot survive either callback order: {item:?}"
    );
    // A late duplicate callback cannot resurrect the rejected admission either.
    if let Some(id) = target {
        m.feed(A, Input::InjectDone { id, ok: true });
    }
    assert!(m.physical_trace(A, item).is_empty());
    m.defer_targeting = false;
    m.defer_releases = false;
    m.proxy_item(C, c, item, false);
    m.log.clear();
    m.proxy_item(C, c, item, true);
    assert_eq!(
        m.physical_trace(A, item),
        vec![true],
        "fresh input uses ordinary admission"
    );
    if matches!(item, Held::Button(_)) {
        assert_eq!(
            m.count(A, |o| matches!(
                o,
                Output::Inject {
                    cmd: InjectCmd::MoveTo { .. },
                    ..
                }
            )),
            1
        );
    }
    m.proxy_item(C, c, item, false);
    assert_eq!(m.physical_trace(A, item), vec![true, false]);
    if matches!(item, Held::Key(_)) {
        m.proxy_item(C, c, Held::Button(MouseButton(2)), false);
    }
    assert!(m.held.is_empty());
    assert!(m.buttons.is_empty());
}

#[test]
fn blocked_button_does_not_survive_cleanup_then_targeting() {
    blocked_down_callback_orders(Held::Button(MouseButton::PRIMARY), true);
}

#[test]
fn blocked_button_does_not_survive_targeting_then_cleanup() {
    blocked_down_callback_orders(Held::Button(MouseButton::PRIMARY), false);
}

#[test]
fn blocked_key_does_not_survive_cleanup_then_targeting() {
    blocked_down_callback_orders(Held::Key(KEY), true);
}

#[test]
fn blocked_key_does_not_survive_targeting_then_cleanup() {
    blocked_down_callback_orders(Held::Key(KEY), false);
}

#[test]
#[ignore = "N0 gap: intentional v0 same-window restriction; N1 fan-out extension"]
fn same_window_can_project_to_two_destinations() {
    let mut m = Mesh::new(false);
    let first = m.project(A, B, WINDOW);
    assert_eq!(first, key(A, 1));
    assert_eq!(
        m.count(A, |o| matches!(o, Output::StartCapture { peer: B, .. })),
        1
    );
    assert_eq!(
        m.count(
            B,
            |o| matches!(o, Output::ProxyGeometry { key, .. } if *key == first)
        ),
        1
    );
    // An actual, live A→B projection owns the parked window. Request A→C through the same
    // public command/network/platform sequence; the v0 Busy refusal prevents this extension.
    m.feed(
        A,
        Input::Command(Command::Project {
            window: WINDOW,
            to: C,
            place: None,
        }),
    );
    assert_eq!(
        m.count(C, |o| matches!(o, Output::OpenProxy { .. })),
        1,
        "N1 fan-out: C must receive the same source window while B remains live; outputs: {:?}",
        m.log
    );
    assert_eq!(m.count(B, |o| matches!(o, Output::CloseProxy { .. })), 0);
}

#[test]
fn clipboard_c_offer_supersedes_as_promise_on_b_without_relay_or_late_fulfilment() {
    let mut m = Mesh::new(false);
    m.enable_clipboard();
    let a = m.project(A, B, WINDOW);
    let c = m.project(C, B, WINDOW);
    assert_eq!(
        a.projection, c.projection,
        "connection-local IDs deliberately collide"
    );
    m.clipboard_focus(B, a, true);
    m.clipboard_copy(A, b"A fixture");
    m.clipboard_focus(B, a, false);
    assert!(m.clipboard[&B].current_promise().is_some());
    assert_eq!(m.clipboard[&A].reads, 0);
    m.delay_clip_reads = true;
    m.clipboard_paste(B, 400);
    assert_eq!(m.clipboard[&A].reads, 1);
    assert!(m.clipboard[&B].answer(LocalPasteId(400)).is_none());

    m.clipboard_focus(B, c, true);
    m.clipboard_copy(C, b"C fixture");
    m.clipboard_focus(B, c, false);
    assert_eq!(m.clipboard[&B].answer(LocalPasteId(400)), Some(&None));
    assert_eq!(m.clipboard[&C].reads, 0);
    for (node, input) in std::mem::take(&mut m.delayed_clip_reads) {
        m.feed(node, input);
    }
    assert_eq!(m.clipboard[&B].answer(LocalPasteId(400)), Some(&None));
    m.delay_clip_reads = false;
    m.clipboard_paste(B, 401);
    assert_eq!(
        m.clipboard[&B].answer(LocalPasteId(401)),
        Some(&Some(b"C fixture".to_vec()))
    );
    assert_eq!(m.clipboard[&A].reads, 1);
    assert_eq!(m.clipboard[&C].reads, 1);
    assert_eq!(m.clipboard[&B].reads, 0);
    assert_eq!(
        m.count(B, |o| matches!(
            o,
            Output::SendControl {
                msg: ControlMessage::ClipOffer(_),
                ..
            }
        )),
        0
    );
}

#[test]
fn clipboard_source_focus_epochs_are_per_projection_and_peer() {
    let mut m = Mesh::new(false);
    m.enable_clipboard();
    let b = m.project(A, B, WINDOW);
    let c = m.project(A, C, WindowId(11));
    m.clipboard_focus(B, b, true);
    m.clipboard_copy(A, b"only B was focused");
    m.clipboard_focus(C, c, false);
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::SendControl {
                msg: ControlMessage::ClipOffer(_),
                ..
            }
        )),
        0
    );
    m.clipboard_focus(B, b, false);
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::SendControl {
                peer: B,
                msg: ControlMessage::ClipOffer(_),
                ..
            }
        )),
        1
    );
    assert!(m.clipboard[&C].current_promise().is_none());
    m.clipboard_focus(C, c, true);
    m.clipboard_copy(A, b"now C is focused");
    m.clipboard_focus(C, c, false);
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::SendControl {
                peer: C,
                msg: ControlMessage::ClipOffer(_),
                ..
            }
        )),
        1
    );
    assert!(m.clipboard[&C].current_promise().is_some());
    assert_eq!(m.clipboard[&A].reads, 0);
}

#[test]
fn clipboard_home_guard_ignores_raw_source_focus_loss() {
    let mut m = Mesh::new(false);
    m.enable_clipboard();
    let b = m.project(A, B, WINDOW);
    m.clipboard_focus(B, b, true);
    m.feed(
        B,
        Input::Proxy {
            key: b,
            event: ProxyEvent::Placed {
                display: Some(DISPLAY),
                origin: PointDevice::new(200.0, 300.0),
                size: PixelSize::new(400, 300),
            },
        },
    );
    m.enter(A, B, 0.5);
    m.motion(A, 250.0, -100.0);
    m.feed(
        B,
        Input::Proxy {
            key: b,
            event: ProxyEvent::Motion {
                position: PointDevice::new(50.0, 100.0),
            },
        },
    );
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::Notice(Notice::Home { entered: true, .. })
        )),
        1
    );
    m.clipboard_copy(A, b"native home copy");
    m.log.clear();
    m.feed(
        A,
        projection(
            B,
            ProjectionMessage::Focus {
                projection: b.projection,
                focused: false,
            },
        ),
    );
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::SendControl {
                msg: ControlMessage::ClipOffer(_),
                ..
            }
        )),
        0
    );
    m.feed(A, Input::Command(Command::ReleaseControl));
    m.clipboard_copy(A, b"copy after leaving home");
    m.feed(
        A,
        projection(
            B,
            ProjectionMessage::Focus {
                projection: b.projection,
                focused: false,
            },
        ),
    );
    assert_eq!(
        m.count(A, |o| matches!(
            o,
            Output::SendControl {
                msg: ControlMessage::ClipOffer(_),
                ..
            }
        )),
        1,
        "ignored focus loss must not clear the accepted focus epoch"
    );
}

fn item_request(m: &Mesh, item: Held, wanted_down: bool) -> crosspane_engine::io::InjectId {
    m.log
        .iter()
        .rev()
        .find_map(|(node, output)| match (item, output) {
            (
                Held::Key(wanted),
                Output::Inject {
                    id,
                    cmd: InjectCmd::Key { usage, down },
                },
            ) if *node == A && wanted == *usage && *down == wanted_down => Some(*id),
            (
                Held::Button(wanted),
                Output::Inject {
                    id,
                    cmd: InjectCmd::Button { button, down },
                },
            ) if *node == A && wanted == *button && *down == wanted_down => Some(*id),
            _ => None,
        })
        .expect("the exact physical item request")
}

#[test]
fn n1e2a_local_override_preserves_shared_projection_items() {
    for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        let mut m = Mesh::new(false);
        let c = m.project(A, C, WINDOW);
        m.enter(B, A, 0.5);
        m.e1_item(B, item, true);
        m.proxy_item(C, c, item, true);
        m.log.clear();
        m.feed(A, Input::Capture(CaptureEvent::LocalActivity { at: ms(0) }));
        assert_eq!(m.engines[&A].controlled_by(), None);
        assert!(m.physical_trace(A, item).is_empty());
        m.feed(C, Input::Command(Command::Return(c)));
        assert_eq!(m.physical_trace(A, item), vec![false]);
        assert!(m.held.is_empty());
        assert!(m.buttons.is_empty());
    }
}

fn cross_role_uncertain_press(e1_first: bool, late_success: bool) {
    for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        let mut m = Mesh::new(false);
        let c = m.project(A, C, WINDOW);
        m.enter(B, A, 0.5);
        m.defer_presses = true;
        m.defer_releases = true;
        m.log.clear();
        if e1_first {
            m.e1_item(B, item, true);
        } else {
            m.proxy_item(C, c, item, true);
        }
        let press = item_request(&m, item, true);
        if e1_first {
            m.proxy_item(C, c, item, true);
        } else {
            m.e1_item(B, item, true);
        }
        assert_eq!(m.physical_trace(A, item), vec![true]);
        m.log.clear();
        if late_success {
            m.now = 501;
        }
        m.feed(
            A,
            Input::InjectDone {
                id: press,
                ok: late_success,
            },
        );
        assert_eq!(m.physical_trace(A, item), vec![false]);
        let cleanup = item_request(&m, item, false);
        m.log.clear();
        // Both owners' old Ups and repeated Downs are absorbed until that exact cleanup confirms.
        m.e1_item(B, item, false);
        m.proxy_item(C, c, item, false);
        m.e1_item(B, item, true);
        m.proxy_item(C, c, item, true);
        assert!(m.physical_trace(A, item).is_empty());
        m.feed(
            A,
            Input::InjectDone {
                id: cleanup,
                ok: true,
            },
        );
        assert!(m.physical_trace(A, item).is_empty());
        m.defer_presses = false;
        m.defer_releases = false;
        m.now += 300; // Fresh activation respects the ordinary focus guard's grace interval.
        m.feed(A, Input::Windows(WindowEvent::Focused(None)));
        m.log.clear();
        m.proxy_item(C, c, item, false);
        m.proxy_item(C, c, item, true);
        assert_eq!(m.physical_trace(A, item), vec![true]);
        assert!(m.log.iter().any(|(node, output)| {
            *node == A
                && match item {
                    Held::Key(_) => {
                        matches!(output, Output::ActivateWindow { window } if *window == WINDOW)
                    }
                    Held::Button(_) => matches!(
                        output,
                        Output::Inject {
                            cmd: InjectCmd::MoveTo { .. },
                            ..
                        }
                    ),
                }
        }));
        m.proxy_item(C, c, item, false);
        assert_eq!(m.physical_trace(A, item), vec![true, false]);
    }
}

#[test]
fn n1e2a_failed_e1_press_cleans_both_roles_once() {
    cross_role_uncertain_press(true, false);
}

#[test]
fn n1e2a_failed_e2_press_cleans_both_roles_once() {
    cross_role_uncertain_press(false, false);
}

#[test]
fn n1e2a_late_e1_press_success_before_tick_still_cleans_both_roles() {
    cross_role_uncertain_press(true, true);
}

#[test]
fn n1e2a_late_e2_press_success_before_tick_still_cleans_both_roles() {
    cross_role_uncertain_press(false, true);
}

#[test]
fn n1e2a_shared_cleanup_confirms_each_journal_independently() {
    for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        for fail_e1 in [false, true] {
            let e1 = RecordedJournal::default();
            let e2 = RecordedJournal::default();
            let mut m = Mesh::with_journals(false, Some((e1.clone(), e2.clone())));
            let c = m.project(A, C, WINDOW);
            let fresh = m.project(A, C, WindowId(11));
            m.enter(B, A, 0.5);
            m.defer_presses = true;
            m.defer_releases = true;
            m.log.clear();
            m.e1_item(B, item, true);
            let press = item_request(&m, item, true);
            m.proxy_item(C, c, item, true);
            m.log.clear();
            m.feed(
                A,
                Input::InjectDone {
                    id: press,
                    ok: false,
                },
            );
            assert_eq!(m.physical_trace(A, item), vec![false]);
            let cleanup = item_request(&m, item, false);
            assert_eq!(e1.held().unwrap(), vec![item]);
            assert_eq!(e2.held().unwrap(), vec![item]);
            let failing = if fail_e1 { &e1 } else { &e2 };
            let other = if fail_e1 { &e2 } else { &e1 };
            failing.0.lock().unwrap().fail_up = true;
            m.log.clear();
            m.feed(
                A,
                Input::InjectDone {
                    id: cleanup,
                    ok: true,
                },
            );
            assert!(m.physical_trace(A, item).is_empty());
            assert!(other.held().unwrap().is_empty());
            assert_eq!(failing.held().unwrap(), vec![item]);
            m.proxy_item(C, fresh, item, true);
            assert!(
                m.physical_trace(A, item).is_empty(),
                "one journal still owes cleanup"
            );
            failing.0.lock().unwrap().fail_up = false;
            m.now = 50;
            m.feed(A, Input::Tick);
            assert_eq!(m.physical_trace(A, item), vec![false]);
            let retry = item_request(&m, item, false);
            m.feed(
                A,
                Input::InjectDone {
                    id: retry,
                    ok: true,
                },
            );
            assert!(e1.held().unwrap().is_empty());
            assert!(e2.held().unwrap().is_empty());
            m.now = 350;
            m.defer_presses = false;
            m.defer_releases = false;
            m.log.clear();
            m.proxy_item(C, fresh, item, false);
            m.proxy_item(C, fresh, item, true);
            assert_eq!(m.physical_trace(A, item), vec![true]);
            m.proxy_item(C, fresh, item, false);
            assert_eq!(m.physical_trace(A, item), vec![true, false]);
            assert!(m.held.is_empty());
            assert!(m.buttons.is_empty());
        }
    }
}

#[test]
fn n1e2a_torn_role_journal_never_releases_the_other_roles_item() {
    for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        for fail_e1 in [false, true] {
            let e1 = RecordedJournal::default();
            let e2 = RecordedJournal::default();
            let mut m = Mesh::with_journals(false, Some((e1.clone(), e2.clone())));
            let c = m.project(A, C, WINDOW);
            m.enter(B, A, 0.5);
            let failing = if fail_e1 { &e1 } else { &e2 };
            if fail_e1 {
                m.proxy_item(C, c, item, true);
            } else {
                m.e1_item(B, item, true);
            }
            failing.0.lock().unwrap().fail_down = true;
            m.log.clear();
            if fail_e1 {
                m.e1_item(B, item, true);
            } else {
                m.proxy_item(C, c, item, true);
            }
            assert!(m.physical_trace(A, item).is_empty());
            assert!(
                failing.held().unwrap().is_empty(),
                "torn role record confirms locally"
            );
            failing.0.lock().unwrap().fail_down = false;
            if fail_e1 {
                m.feed(C, Input::Command(Command::Return(c)));
            } else {
                m.feed(B, Input::Command(Command::ReleaseControl));
            }
            assert_eq!(m.physical_trace(A, item), vec![false]);
            assert!(e1.held().unwrap().is_empty());
            assert!(e2.held().unwrap().is_empty());
            assert!(m.held.is_empty());
            assert!(m.buttons.is_empty());
        }
    }
}

#[test]
fn n1e2a_expired_cross_role_press_cleans_before_ordinary_input() {
    for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        for e1_first in [false, true] {
            let mut m = Mesh::new(false);
            let c = m.project(A, C, WINDOW);
            m.enter(B, A, 0.5);
            m.defer_presses = true;
            m.defer_releases = true;
            if e1_first {
                m.e1_item(B, item, true);
                m.proxy_item(C, c, item, true);
            } else {
                m.proxy_item(C, c, item, true);
                m.e1_item(B, item, true);
            }
            m.log.clear();
            m.now = 501;
            // No Tick or callback: admission of an ordinary event must first expire the press.
            m.feed(A, Input::Windows(WindowEvent::Focused(Some(WINDOW))));
            assert_eq!(m.physical_trace(A, item), vec![false]);
            let cleanup = item_request(&m, item, false);
            m.log.clear();
            m.feed(
                A,
                Input::InjectDone {
                    id: cleanup,
                    ok: true,
                },
            );
            assert!(m.physical_trace(A, item).is_empty());
            assert!(m.held.is_empty());
            assert!(m.buttons.is_empty());
        }
    }
}

#[test]
fn n1e2a_shared_cleanup_history_keeps_eight_actual_request_ids() {
    for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        let e1 = RecordedJournal::default();
        let e2 = RecordedJournal::default();
        let mut m = Mesh::with_journals(false, Some((e1.clone(), e2.clone())));
        let c = m.project(A, C, WINDOW);
        m.enter(B, A, 0.5);
        m.defer_presses = true;
        m.defer_releases = true;
        m.e1_item(B, item, true);
        let press = item_request(&m, item, true);
        m.proxy_item(C, c, item, true);
        m.feed(
            A,
            Input::InjectDone {
                id: press,
                ok: false,
            },
        );
        let first = item_request(&m, item, false);
        let mut recent = VecDeque::from([first]);
        for attempt in 1..=20 {
            m.now = attempt * 50;
            m.log.clear();
            m.feed(A, Input::Tick);
            assert_eq!(m.physical_trace(A, item), vec![false]);
            recent.push_back(item_request(&m, item, false));
            if recent.len() > 8 {
                recent.pop_front();
            }
            assert_eq!(e1.held().unwrap(), vec![item]);
            assert_eq!(e2.held().unwrap(), vec![item]);
        }
        m.log.clear();
        m.feed(
            A,
            Input::InjectDone {
                id: first,
                ok: true,
            },
        );
        assert_eq!(e1.held().unwrap(), vec![item]);
        assert_eq!(e2.held().unwrap(), vec![item]);
        m.feed(
            A,
            Input::InjectDone {
                id: recent[0],
                ok: true,
            },
        );
        assert!(e1.held().unwrap().is_empty());
        assert!(e2.held().unwrap().is_empty());
        m.feed(
            A,
            Input::InjectDone {
                id: *recent.back().unwrap(),
                ok: false,
            },
        );
        m.now += 50;
        m.feed(A, Input::Tick);
        assert!(m.physical_trace(A, item).is_empty());
    }
}

#[test]
fn n1e2a_old_role_release_retry_is_absorbed_by_a_fresh_other_role_hold() {
    for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        for e1_first in [false, true] {
            let mut m = Mesh::new(false);
            let c = m.project(A, C, WINDOW);
            m.enter(B, A, 0.5);
            m.defer_releases = true;
            if e1_first {
                m.e1_item(B, item, true);
                m.e1_item(B, item, false);
            } else {
                m.proxy_item(C, c, item, true);
                m.proxy_item(C, c, item, false);
            }
            let old = item_request(&m, item, false);
            m.log.clear();
            if e1_first {
                m.proxy_item(C, c, item, true);
            } else {
                m.e1_item(B, item, true);
            }
            assert_eq!(m.physical_trace(A, item), vec![true]);
            m.log.clear();
            m.feed(A, Input::InjectDone { id: old, ok: true });
            m.now = 50;
            m.feed(A, Input::Tick);
            assert!(m.physical_trace(A, item).is_empty());
            m.defer_releases = false;
            if e1_first {
                m.proxy_item(C, c, item, false);
            } else {
                m.e1_item(B, item, false);
            }
            assert_eq!(m.physical_trace(A, item), vec![false]);
            assert!(m.held.is_empty());
            assert!(m.buttons.is_empty());
        }
    }
}

fn recovery_requests(m: &Mesh) -> Vec<InjectId> {
    m.log
        .iter()
        .filter_map(|(node, out)| match out {
            Output::Inject {
                id,
                cmd: InjectCmd::Recover { .. },
            } if *node == A => Some(*id),
            _ => None,
        })
        .collect()
}

#[test]
fn n1e2b_startup_recovers_each_union_item_once() {
    for e1_items in [
        vec![],
        vec![Held::Key(KEY)],
        vec![Held::Key(KEY), Held::Button(MouseButton::PRIMARY)],
    ] {
        for e2_items in [
            vec![],
            vec![Held::Key(KEY)],
            vec![Held::Button(MouseButton::PRIMARY)],
        ] {
            let e1 = RecordedJournal::default();
            let e2 = RecordedJournal::default();
            e1.0.lock().unwrap().held.extend(&e1_items);
            e2.0.lock().unwrap().held.extend(&e2_items);
            let mut m = Mesh::with_startup_journals(false, Some((e1.clone(), e2.clone())), true);
            let union: BTreeSet<_> = e1_items.iter().chain(&e2_items).copied().collect();
            for item in &union {
                assert_eq!(m.physical_trace(A, *item), vec![false]);
            }
            assert_eq!(e1.held().unwrap(), e1_items);
            assert_eq!(e2.held().unwrap(), e2_items);
            let requests = recovery_requests(&m);
            for id in requests {
                m.feed(A, Input::InjectDone { id, ok: true });
            }
            assert!(e1.held().unwrap().is_empty());
            assert!(e2.held().unwrap().is_empty());
        }
    }
}

#[test]
fn n1e2b_startup_in_one_journal_blocks_establishing_downs_in_both_roles() {
    for crashed in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        let e1 = RecordedJournal::default();
        let e2 = RecordedJournal::default();
        e1.0.lock().unwrap().held.insert(crashed);
        let mut m = Mesh::with_startup_journals(false, Some((e1.clone(), e2)), true);
        let recovery = recovery_requests(&m)[0];
        let c = m.project(A, C, WINDOW);
        m.enter(B, A, 0.5);
        let fresh = Held::Key(HidUsage::keyboard(5));
        m.log.clear();
        m.e1_item(B, fresh, true);
        m.proxy_item(C, c, fresh, true);
        assert!(m.physical_trace(A, fresh).is_empty());
        assert_eq!(e1.held().unwrap(), vec![crashed]);
        m.feed(
            A,
            Input::InjectDone {
                id: recovery,
                ok: true,
            },
        );
        m.e1_item(B, fresh, false);
        m.proxy_item(C, c, fresh, false);
        m.e1_item(B, fresh, true);
        m.proxy_item(C, c, fresh, true);
        assert_eq!(m.physical_trace(A, fresh), vec![true]);
        m.e1_item(B, fresh, false);
        m.proxy_item(C, c, fresh, false);
        assert_eq!(m.physical_trace(A, fresh), vec![true, false]);
    }
}

#[test]
fn n1e2b_startup_shared_success_confirms_journals_independently() {
    for fail_e1 in [true, false] {
        for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
            let e1 = RecordedJournal::default();
            let e2 = RecordedJournal::default();
            e1.0.lock().unwrap().held.insert(item);
            e2.0.lock().unwrap().held.insert(item);
            let mut m = Mesh::with_startup_journals(false, Some((e1.clone(), e2.clone())), true);
            assert_eq!(m.physical_trace(A, item), vec![false]);
            let initial = recovery_requests(&m)[0];
            let failing = if fail_e1 { &e1 } else { &e2 };
            let other = if fail_e1 { &e2 } else { &e1 };
            failing.0.lock().unwrap().fail_up = true;
            m.feed(
                A,
                Input::InjectDone {
                    id: initial,
                    ok: true,
                },
            );
            assert_eq!(failing.held().unwrap(), vec![item]);
            assert!(other.held().unwrap().is_empty());
            m.enter(B, A, 0.5);
            let fresh = Held::Key(HidUsage::keyboard(5));
            m.log.clear();
            m.e1_item(B, fresh, true);
            assert!(m.physical_trace(A, fresh).is_empty());
            m.now = 50;
            m.feed(A, Input::Tick);
            assert_eq!(m.physical_trace(A, item), vec![false]);
            let retry = recovery_requests(&m)[0];
            failing.0.lock().unwrap().fail_up = false;
            m.feed(
                A,
                Input::InjectDone {
                    id: retry,
                    ok: true,
                },
            );
            assert!(failing.held().unwrap().is_empty());
            m.e1_item(B, fresh, false);
            m.e1_item(B, fresh, true);
            assert_eq!(m.physical_trace(A, fresh), vec![true]);
            m.e1_item(B, fresh, false);
            assert_eq!(m.physical_trace(A, fresh), vec![true, false]);
        }
    }
}

#[test]
fn n1e2b_unanswered_startup_retains_exact_bounded_requests() {
    let e1 = RecordedJournal::default();
    let e2 = RecordedJournal::default();
    let items = [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)];
    e1.0.lock().unwrap().held.extend(items);
    e2.0.lock().unwrap().held.extend(items);
    let mut m = Mesh::with_startup_journals(false, Some((e1.clone(), e2.clone())), true);
    assert_eq!(recovery_requests(&m).len(), 1);
    let mut requests = recovery_requests(&m);
    for n in 1..=20 {
        m.log.clear();
        m.now = n * 50;
        m.feed(A, Input::Tick);
        let retry = recovery_requests(&m);
        assert_eq!(retry.len(), 1);
        requests.extend(retry);
        for item in items {
            assert_eq!(m.physical_trace(A, item), vec![false]);
        }
    }
    m.feed(
        A,
        Input::InjectDone {
            id: requests[0],
            ok: true,
        },
    );
    assert_eq!(e1.held().unwrap(), items);
    assert_eq!(e2.held().unwrap(), items);
    m.feed(
        A,
        Input::InjectDone {
            id: requests[requests.len() - 8],
            ok: true,
        },
    );
    assert!(e1.held().unwrap().is_empty());
    assert!(e2.held().unwrap().is_empty());
    m.feed(
        A,
        Input::InjectDone {
            id: *requests.last().unwrap(),
            ok: false,
        },
    );
    m.log.clear();
    m.now += 50;
    m.feed(A, Input::Tick);
    assert!(recovery_requests(&m).is_empty());
}

#[test]
fn n1e2b_read_failure_in_either_journal_returns_no_engine() {
    for fail_e1 in [true, false] {
        let e1 = RecordedJournal::default();
        let e2 = RecordedJournal::default();
        e1.0.lock().unwrap().held.insert(Held::Key(KEY));
        e2.0.lock()
            .unwrap()
            .held
            .insert(Held::Button(MouseButton::PRIMARY));
        if fail_e1 {
            e1.0.lock().unwrap().fail_read = true;
        } else {
            e2.0.lock().unwrap().fail_read = true;
        }
        assert!(
            Engine::new(
                EngineConfig::new(A),
                Box::new(e1.clone()),
                Box::new(e2.clone()),
                ms(0)
            )
            .is_err()
        );
        assert_eq!(e1.0.lock().unwrap().held, BTreeSet::from([Held::Key(KEY)]));
        assert_eq!(
            e2.0.lock().unwrap().held,
            BTreeSet::from([Held::Button(MouseButton::PRIMARY)])
        );
    }
}

#[test]
fn n1e2b_failed_startup_backs_off_shared_cleanup_without_any_down() {
    for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        for roles in [[true, false], [false, true], [true, true]] {
            let e1 = RecordedJournal::default();
            let e2 = RecordedJournal::default();
            if roles[0] {
                e1.0.lock().unwrap().held.insert(item);
            }
            if roles[1] {
                e2.0.lock().unwrap().held.insert(item);
            }
            let mut m = Mesh::with_startup_journals(false, Some((e1.clone(), e2.clone())), true);
            let first = recovery_requests(&m);
            assert_eq!(first.len(), 1);
            m.enter(B, A, 0.5);
            m.now = 7;
            m.feed(
                A,
                Input::InjectDone {
                    id: first[0],
                    ok: false,
                },
            );
            m.log.clear();
            let fresh = Held::Key(HidUsage::keyboard(5));
            m.e1_item(B, fresh, true);
            assert!(m.physical_trace(A, fresh).is_empty());
            m.now = 56;
            m.feed(A, Input::Tick);
            assert!(recovery_requests(&m).is_empty());
            m.now = 57;
            m.feed(A, Input::Tick);
            let retry = recovery_requests(&m);
            assert_eq!(retry.len(), 1);
            assert_ne!(first, retry);
            assert_eq!(m.physical_trace(A, item), vec![false]);
            // A superseded failure must not shift the current item's retry deadline.
            m.now = 58;
            m.feed(
                A,
                Input::InjectDone {
                    id: first[0],
                    ok: false,
                },
            );
            m.feed(
                A,
                Input::InjectDone {
                    id: retry[0],
                    ok: true,
                },
            );
            assert!(e1.held().unwrap().is_empty());
            assert!(e2.held().unwrap().is_empty());
            m.e1_item(B, fresh, false);
            m.e1_item(B, fresh, true);
            m.e1_item(B, fresh, false);
            assert_eq!(m.physical_trace(A, fresh), vec![true, false]);
        }
    }
}

#[test]
fn n1e2b_startup_rejects_interleaved_non_recovery_acknowledgments() {
    for item in [Held::Key(KEY), Held::Button(MouseButton::PRIMARY)] {
        let e1 = RecordedJournal::default();
        let e2 = RecordedJournal::default();
        e1.0.lock().unwrap().held.insert(item);
        e2.0.lock().unwrap().held.insert(item);
        let mut m = Mesh::with_startup_journals(false, Some((e1.clone(), e2.clone())), true);
        let first = recovery_requests(&m)[0];
        m.defer_targeting = true;
        m.enter(B, A, 0.5);
        let unrelated = m
            .log
            .iter()
            .find_map(|(node, output)| match output {
                Output::Inject {
                    id,
                    cmd: InjectCmd::MoveTo { .. },
                } if *node == A => Some(*id),
                _ => None,
            })
            .unwrap();
        assert_ne!(first, unrelated);
        m.log.clear();
        m.now = 50;
        m.feed(A, Input::Tick);
        let current = recovery_requests(&m)[0];
        assert_ne!(current, unrelated);
        m.feed(
            A,
            Input::InjectDone {
                id: unrelated,
                ok: true,
            },
        );
        assert_eq!(e1.held().unwrap(), vec![item]);
        assert_eq!(e2.held().unwrap(), vec![item]);
        m.feed(
            A,
            Input::InjectDone {
                id: current,
                ok: true,
            },
        );
        assert!(e1.held().unwrap().is_empty());
        assert!(e2.held().unwrap().is_empty());
    }
}
