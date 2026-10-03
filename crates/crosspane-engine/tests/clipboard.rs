#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use crosspane_engine::io::ClipBytes;
use crosspane_engine::{Command, Engine, EngineConfig, Input, Output};
use crosspane_input::journal::MemoryJournal;
use crosspane_platform::{
    CaptureEvent, CaptureStart, ClipKinds, ClipboardEvent, ClipboardHost, Edge, EndReason, IoGate,
    LockState, OverlayEvent, PortalId, SessionEvent, SessionState,
};
use crosspane_protocol::clip::{MAX_CLIP_IMAGE, MAX_CLIP_TEXT};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{
    Capability, ClipFailure, ClipFetch, ClipFetchFailed, ClipFetchId, ClipOffer, ClipOfferId,
    ClipWithdraw, ControlMessage, Placement,
};
use crosspane_testkit::FakeClipboardHost;
use crosspane_types::{
    ClipKind,
    color::ColorSpace,
    display::DisplayInfo,
    geom::{DisplayGeometry, PixelSize, PointLogical, PointMm, SizeMm},
    id::{DisplayId, NodeId},
    input::LockKeys,
    time::MonoTime,
};

const NODES: [NodeId; 3] = [NodeId([1; 32]), NodeId([2; 32]), NodeId([3; 32])];
const TEXT: ClipKinds = ClipKinds {
    text: true,
    image: false,
};
fn ms(n: u64) -> MonoTime {
    MonoTime::from_nanos(n * 1_000_000)
}
fn control(peer: NodeId, msg: ControlMessage) -> Input {
    Input::Link(LinkEvent::Control { peer, msg })
}
fn open() -> Input {
    state(LockState::Unlocked)
}
fn state(lock: LockState) -> Input {
    Input::Session(SessionEvent::State(SessionState {
        lock,
        active: Some(true),
    }))
}
fn clip_output(o: &Output) -> bool {
    matches!(
        o,
        Output::ClipRead { .. }
            | Output::ClipPromise { .. }
            | Output::ClipWithdraw { .. }
            | Output::ClipFulfil { .. }
            | Output::SendClipData { .. }
            | Output::SendControl {
                msg: ControlMessage::ClipOffer(_)
                    | ControlMessage::ClipWithdraw(_)
                    | ControlMessage::ClipFetch(_)
                    | ControlMessage::ClipFetchFailed(_),
                ..
            }
    )
}
fn only(out: Vec<Output>) -> Vec<Output> {
    out.into_iter().filter(clip_output).collect()
}
fn engine(node: NodeId) -> Engine {
    let (mut e, _) = Engine::new(
        EngineConfig::new(node),
        Box::<MemoryJournal>::default(),
        Box::<MemoryJournal>::default(),
        ms(0),
    )
    .unwrap();
    e.handle(open(), ms(0));
    e
}
fn grant(read: bool, write: bool) -> Input {
    let mut caps = BTreeSet::from([Capability::InputAccept]);
    if read {
        caps.insert(Capability::ClipboardRead);
    }
    if write {
        caps.insert(Capability::ClipboardWrite);
    }
    Input::Grants(NODES.into_iter().map(|p| (p, caps.clone())).collect())
}
fn prepared() -> Engine {
    let mut e = engine(NODES[1]);
    for peer in [NODES[0], NODES[2]] {
        e.handle(Input::PeerUp { peer }, ms(0));
        e.handle(
            Input::ClipPeer {
                peer,
                available: true,
            },
            ms(0),
        );
    }
    e.handle(grant(true, true), ms(0));
    e
}
fn remote_offer(id: u64, kinds: Vec<ClipKind>) -> Input {
    control(
        NODES[0],
        ControlMessage::ClipOffer(ClipOffer {
            offer: ClipOfferId(id),
            kinds,
        }),
    )
}
fn promise(e: &mut Engine, id: u64) -> u64 {
    only(e.handle(remote_offer(id, vec![ClipKind::Text]), ms(0)))
        .into_iter()
        .find_map(|o| match o {
            Output::ClipPromise { offer, .. } => Some(offer),
            _ => None,
        })
        .unwrap()
}
fn paste(e: &mut Engine, offer: u64, id: u64, now: u64) -> ClipFetchId {
    only(e.handle(
        Input::Clipboard(ClipboardEvent::PasteRequested {
            paste: crosspane_platform::LocalPasteId(id),
            offer,
            kind: ClipKind::Text,
        }),
        ms(now),
    ))
    .into_iter()
    .find_map(|o| match o {
        Output::SendControl {
            msg: ControlMessage::ClipFetch(f),
            ..
        } => Some(f.fetch),
        _ => None,
    })
    .unwrap()
}
fn data(fetch: ClipFetchId, text: &[u8]) -> Input {
    Input::ClipData {
        peer: NODES[0],
        fetch,
        kind: ClipKind::Text,
        data: ClipBytes(text.to_vec()),
    }
}
fn empty(out: &[Output], paste: u64) -> bool {
    out.iter()
        .any(|o| matches!(o, Output::ClipFulfil { paste: p, data: None } if p.0 == paste))
}
fn has_withdraw(out: &[Output]) -> bool {
    out.iter().any(|o| matches!(o, Output::ClipWithdraw { .. }))
}

/// Runs actual E1 acceptance, lazy host operations and protocol delivery; no native I/O.
struct World {
    engines: Vec<Engine>,
    hosts: Vec<FakeClipboardHost>,
    gates: Vec<Arc<IoGate>>,
    pending: Arc<Mutex<VecDeque<(usize, Input)>>>,
    delayed: Vec<(usize, Input)>,
    delay_reads: bool,
    trace: Vec<(usize, Output)>,
    portals: Vec<Vec<crosspane_platform::CapturePortal>>,
    captures: Vec<Option<crosspane_platform::CaptureId>>,
    now: u64,
}
impl World {
    fn new(count: usize) -> Self {
        let pending = Arc::new(Mutex::new(VecDeque::new()));
        let mut hosts = Vec::new();
        let mut gates = Vec::new();
        for n in 0..count {
            let q = Arc::clone(&pending);
            let gate = IoGate::new();
            gate.set_session_permits(true);
            gate.set_engine_permits(true);
            let mut h = FakeClipboardHost::new(Arc::clone(&gate));
            h.subscribe(Arc::new(move |event| {
                q.lock().unwrap().push_back((n, Input::Clipboard(event)))
            }))
            .unwrap();
            hosts.push(h);
            gates.push(gate);
        }
        let mut w = Self {
            engines: NODES[..count].iter().map(|n| engine(*n)).collect(),
            hosts,
            gates,
            pending,
            delayed: Vec::new(),
            delay_reads: false,
            trace: Vec::new(),
            portals: vec![Vec::new(); count],
            captures: vec![None; count],
            now: 0,
        };
        let display = DisplayInfo {
            id: DisplayId(1),
            name: "fixture".into(),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(100.0, 100.0),
                pixel_size: PixelSize::new(1000, 1000),
                scale: 1.0,
                logical_origin: PointLogical::zero(),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        };
        for (n, node) in NODES.iter().enumerate().take(count) {
            w.feed(n, Input::LocalDisplays(vec![display.clone()]));
            for peer in NODES[..count].iter().copied().filter(|p| p != node) {
                w.feed(
                    n,
                    Input::PeerDisplays {
                        peer,
                        displays: vec![display.clone()],
                    },
                );
                w.feed(n, Input::PeerUp { peer });
                w.feed(
                    n,
                    Input::ClipPeer {
                        peer,
                        available: true,
                    },
                );
            }
            w.feed(n, grant(true, true));
            w.feed(
                n,
                Input::Layout(
                    NODES[..count]
                        .iter()
                        .enumerate()
                        .map(|(i, peer)| Placement {
                            node: *peer,
                            display: DisplayId(1),
                            origin: PointMm::new(i as f64 * 100.0, 0.0),
                            version: 1,
                        })
                        .collect(),
                ),
            );
        }
        w.trace.clear();
        w
    }
    fn queue(&self, node: usize, input: Input) {
        self.pending.lock().unwrap().push_back((node, input));
    }
    fn feed(&mut self, n: usize, i: Input) {
        self.queue(n, i);
        self.pump();
    }
    fn pump(&mut self) {
        for _ in 0..1000 {
            let next = self.pending.lock().unwrap().pop_front();
            let Some((n, input)) = next else {
                return;
            };
            if let Input::Session(event) = &input {
                self.gates[n].set_session_permits(match event {
                    SessionEvent::State(state) => state.permits_io(),
                    _ => false,
                });
            }
            let out = self.engines[n].handle(input, ms(self.now));
            for o in out {
                self.trace.push((n, o.clone()));
                self.execute(n, o);
            }
        }
        panic!("simulation did not quiesce");
    }
    fn execute(&mut self, n: usize, o: Output) {
        match o {
            Output::EngineGate(permits) => self.gates[n].set_engine_permits(permits),
            Output::SetPortals(p) => {
                let ids = p.iter().map(|p| p.id).collect();
                self.portals[n] = p;
                self.queue(
                    n,
                    Input::PortalsSet {
                        ids,
                        result: Ok(()),
                    },
                );
            }
            Output::ShowOverlay { id, .. } => {
                self.queue(n, Input::Overlay(OverlayEvent::Visible(id)))
            }
            Output::Inject { id, .. } => self.queue(n, Input::InjectDone { id, ok: true }),
            Output::BeginCapture { id, .. } => {
                self.captures[n] = Some(id);
                self.queue(n, Input::Capture(CaptureEvent::Started { id }));
                self.queue(
                    n,
                    Input::CaptureBegun {
                        id,
                        result: Ok(CaptureStart {
                            held_keys: vec![],
                            lock_keys: LockKeys::default(),
                        }),
                    },
                );
            }
            Output::EndCapture { .. } => {
                if let Some(id) = self.captures[n].take() {
                    self.queue(
                        n,
                        Input::Capture(CaptureEvent::Ended {
                            id,
                            reason: EndReason::Requested,
                        }),
                    );
                }
            }
            Output::SendControl { peer, msg } => self.queue(
                NODES.iter().position(|p| *p == peer).unwrap(),
                control(NODES[n], msg),
            ),
            Output::SendInput { peer, msg } => self.queue(
                NODES.iter().position(|p| *p == peer).unwrap(),
                Input::Link(LinkEvent::Input {
                    peer: NODES[n],
                    msg,
                }),
            ),
            Output::ClipPromise { offer, kinds } => self.hosts[n].promise(offer, kinds).unwrap(),
            Output::ClipWithdraw { offer } => self.hosts[n].withdraw(offer).unwrap(),
            Output::ClipFulfil { paste, data } => self.hosts[n].fulfil(paste, data.map(|d| d.0)),
            Output::ClipRead {
                peer,
                fetch,
                kind,
                max_bytes,
            } => {
                let result =
                    self.hosts[n]
                        .read(kind, max_bytes)
                        .map(ClipBytes)
                        .map_err(|e| match e {
                            crosspane_platform::PlatformError::TooLarge => ClipFailure::TooLarge,
                            crosspane_platform::PlatformError::Locked => ClipFailure::Locked,
                            _ => ClipFailure::Unavailable,
                        });
                let i = Input::ClipReadDone {
                    peer,
                    fetch,
                    result,
                };
                if self.delay_reads {
                    self.delayed.push((n, i));
                } else {
                    self.queue(n, i);
                }
            }
            Output::SendClipData {
                peer,
                fetch,
                kind,
                data,
            } => self.queue(
                NODES.iter().position(|p| *p == peer).unwrap(),
                Input::ClipData {
                    peer: NODES[n],
                    fetch,
                    kind,
                    data,
                },
            ),
            _ => {}
        }
    }
    fn copy(&mut self, n: usize, bytes: &[u8]) {
        self.hosts[n].copy(Some(bytes.to_vec()), None);
        self.pump();
    }
    fn crossing(&mut self, from: usize, to: usize) {
        let edge = if to > from { Edge::Right } else { Edge::Left };
        let p: PortalId = self.portals[from]
            .iter()
            .find(|p| p.edge == edge)
            .unwrap()
            .id;
        self.feed(
            from,
            Input::Capture(CaptureEvent::EdgePressed {
                portal: p,
                position: 0.5,
                at: ms(self.now),
            }),
        );
        assert_eq!(self.engines[from].control_established(), Some(NODES[to]));
        assert_eq!(self.engines[to].controlled_by(), Some(NODES[from]));
    }
    fn release(&mut self, n: usize) {
        self.feed(n, Input::Command(Command::ReleaseControl));
    }
    fn paste(&mut self, n: usize, id: u64) {
        self.hosts[n].paste(crosspane_platform::LocalPasteId(id), ClipKind::Text);
        self.pump();
    }
    fn answer(&self, n: usize, id: u64) -> Option<&Option<Vec<u8>>> {
        self.hosts[n].answer(crosspane_platform::LocalPasteId(id))
    }
    fn flush_reads(&mut self) {
        for (n, i) in std::mem::take(&mut self.delayed) {
            self.queue(n, i);
        }
        self.pump();
    }
}

#[test]
fn e1_crossing_round_trip_is_lazy_and_reads_once_per_paste() {
    let mut w = World::new(2);
    w.copy(0, b"A fixture");
    w.crossing(0, 1);
    assert!(w.hosts[1].current_promise().is_some());
    assert_eq!(w.hosts[0].reads, 0);
    w.paste(1, 10);
    assert_eq!(w.answer(1, 10), Some(&Some(b"A fixture".to_vec())));
    assert_eq!(w.hosts[0].reads, 1);
    w.paste(1, 11);
    assert_eq!(w.answer(1, 11), Some(&Some(b"A fixture".to_vec())));
    assert_eq!(w.hosts[0].reads, 2);
}
#[test]
fn e1_return_round_trip_offers_the_targets_changed_epoch() {
    let mut w = World::new(2);
    w.crossing(0, 1);
    w.copy(1, b"B fixture");
    assert!(w.hosts[0].current_promise().is_some());
    w.release(0);
    assert_eq!(w.engines[1].controlled_by(), None);
    w.paste(0, 12);
    assert_eq!(w.answer(0, 12), Some(&Some(b"B fixture".to_vec())));
    assert_eq!(w.hosts[1].reads, 1);
    assert_eq!(
        w.trace
            .iter()
            .filter(|(n, o)| *n == 1
                && matches!(
                    o,
                    Output::SendControl {
                        msg: ControlMessage::ClipOffer(_),
                        ..
                    }
                ))
            .count(),
        1
    );
}
#[test]
fn target_return_trigger_offers_copy_made_before_control() {
    let mut w = World::new(2);
    w.copy(1, b"B prior copy");
    w.crossing(0, 1);
    assert!(w.hosts[0].current_promise().is_none());
    w.release(0);
    assert!(w.hosts[0].current_promise().is_some());
    w.paste(0, 13);
    assert_eq!(w.answer(0, 13), Some(&Some(b"B prior copy".to_vec())));
}
#[test]
fn either_clipboard_grant_off_moves_nothing() {
    for (node, read, write) in [(0, false, true), (1, true, false)] {
        let mut w = World::new(2);
        w.feed(node, grant(read, write));
        w.copy(0, b"private fixture");
        w.crossing(0, 1);
        assert!(w.hosts[1].current_promise().is_none());
        assert_eq!(w.hosts[0].reads, 0);
    }
}
#[test]
fn absent_feature_accepts_and_sends_nothing() {
    for node in 0..2 {
        let mut w = World::new(2);
        w.feed(
            node,
            Input::ClipPeer {
                peer: NODES[1 - node],
                available: false,
            },
        );
        w.copy(0, b"fixture");
        w.crossing(0, 1);
        assert!(w.hosts[1].current_promise().is_none());
        assert_eq!(w.hosts[0].reads, 0);
        let out = w.engines[node].handle(
            control(
                NODES[1 - node],
                ControlMessage::ClipOffer(ClipOffer {
                    offer: ClipOfferId(80),
                    kinds: vec![ClipKind::Text],
                }),
            ),
            ms(0),
        );
        assert!(only(out).is_empty());
    }
    let mut w = World::new(2);
    for node in 0..2 {
        w.feed(
            node,
            Input::ClipPeer {
                peer: NODES[1 - node],
                available: false,
            },
        );
    }
    w.trace.clear();
    w.copy(0, b"fixture");
    w.crossing(0, 1);
    assert!(!w.trace.iter().any(|(_, o)| clip_output(o)));
}
#[test]
fn revocation_mid_fetch_answers_empty_and_drops_late_reads() {
    for node in 0..2 {
        let mut w = World::new(2);
        w.copy(0, b"fixture");
        w.crossing(0, 1);
        w.delay_reads = true;
        w.paste(1, 20);
        assert_eq!(w.hosts[0].reads, 1);
        w.feed(node, grant(node != 0, node != 1));
        assert_eq!(w.answer(1, 20), Some(&None));
        assert!(w.hosts[1].current_promise().is_none());
        w.flush_reads();
        assert_eq!(w.answer(1, 20), Some(&None));
        assert!(
            !w.trace
                .iter()
                .any(|(_, o)| matches!(o, Output::ClipFulfil { data: Some(_), .. }))
        );
        // The holder cannot observe receiver-side write revocation: an admitted in-flight
        // stream may finish, but the receiver has already answered empty and drops its data.
        if node == 0 {
            assert!(
                !w.trace
                    .iter()
                    .any(|(_, o)| matches!(o, Output::SendClipData { .. }))
            );
        }
    }
}
#[test]
fn locked_or_unknown_mid_fetch_on_either_side_withdraws_and_answers_empty() {
    for node in 0..2 {
        for lock in [LockState::Locked, LockState::Unknown] {
            let mut w = World::new(2);
            w.copy(0, b"fixture");
            w.crossing(0, 1);
            w.delay_reads = true;
            w.paste(1, 21);
            w.feed(node, state(lock));
            assert!(w.hosts[1].current_promise().is_none());
            assert_eq!(w.answer(1, 21), Some(&None));
            w.flush_reads();
            assert_eq!(w.answer(1, 21), Some(&None));
            assert!(
                !w.trace
                    .iter()
                    .any(|(_, o)| matches!(o, Output::ClipFulfil { data: Some(_), .. }))
            );
            // Receiver-side lock is not a cancellation message to the holder. Its late data
            // is harmless: the locked receiver has withdrawn and answered the paste empty.
            if node == 0 {
                assert!(
                    !w.trace
                        .iter()
                        .any(|(_, o)| matches!(o, Output::SendClipData { .. }))
                );
            }
        }
    }
}
#[test]
fn expiry_is_two_seconds_and_late_data_is_dropped() {
    let mut e = prepared();
    let p = promise(&mut e, 1);
    let f = paste(&mut e, p, 30, 100);
    assert_eq!(e.next_deadline(), Some(ms(2100)));
    assert!(!empty(&e.handle(Input::Tick, ms(2099)), 30));
    assert!(empty(&e.handle(Input::Tick, ms(2100)), 30));
    assert!(only(e.handle(data(f, b"late"), ms(2101))).is_empty());
}
#[test]
fn expiry_precedes_data_without_a_tick() {
    let mut e = prepared();
    let p = promise(&mut e, 1);
    let f = paste(&mut e, p, 31, 0);
    let out = only(e.handle(data(f, b"late"), ms(2000)));
    assert!(empty(&out, 31));
    assert_eq!(out.len(), 1);
}
#[test]
fn replayed_stale_and_invalid_offers_are_ignored() {
    let mut e = prepared();
    promise(&mut e, 7);
    for (id, kinds) in [
        (7, vec![ClipKind::Text]),
        (6, vec![ClipKind::Text]),
        (8, vec![]),
        (9, vec![ClipKind::Text, ClipKind::Text]),
        (10, vec![ClipKind::Text, ClipKind::Image, ClipKind::Text]),
    ] {
        assert!(only(e.handle(remote_offer(id, kinds), ms(0))).is_empty());
    }
    let p = promise(&mut e, 8);
    assert_eq!(p, 2);
}
#[test]
fn superseding_a_promise_answers_old_pastes_empty() {
    let mut e = prepared();
    let old = promise(&mut e, 1);
    let f = paste(&mut e, old, 40, 0);
    let out = only(e.handle(remote_offer(2, vec![ClipKind::Image]), ms(0)));
    assert!(has_withdraw(&out));
    assert!(empty(&out, 40));
    assert!(only(e.handle(data(f, b"old"), ms(0))).is_empty());
    let out = e.handle(
        Input::Clipboard(ClipboardEvent::PasteRequested {
            paste: crosspane_platform::LocalPasteId(41),
            offer: old,
            kind: ClipKind::Text,
        }),
        ms(0),
    );
    assert!(empty(&out, 41));
}
#[test]
fn changed_withdraws_current_offer_before_its_replacement() {
    let mut w = World::new(2);
    w.copy(0, b"first");
    w.crossing(0, 1);
    w.trace.clear();
    w.copy(0, b"second");
    let clips: Vec<_> = w
        .trace
        .iter()
        .filter(|(n, o)| *n == 0 && clip_output(o))
        .collect();
    assert!(matches!(
        &clips[0].1,
        Output::SendControl {
            msg: ControlMessage::ClipWithdraw(_),
            ..
        }
    ));
    assert!(matches!(
        &clips[1].1,
        Output::SendControl {
            msg: ControlMessage::ClipOffer(_),
            ..
        }
    ));
    w.paste(1, 42);
    assert_eq!(w.answer(1, 42), Some(&Some(b"second".to_vec())));
}
#[test]
fn peer_promise_never_echoes_back_or_relays_onward() {
    let mut w = World::new(3);
    w.copy(1, b"old native B");
    w.copy(0, b"A fixture");
    w.crossing(0, 1);
    w.release(0);
    assert!(w.hosts[1].current_promise().is_some());
    assert!(!w.trace.iter().any(|(n, o)| *n == 1
        && matches!(
            o,
            Output::SendControl {
                msg: ControlMessage::ClipOffer(_),
                ..
            }
        )));
    w.trace.clear();
    w.now = 200;
    let portal = w.portals[1]
        .iter()
        .find(|p| p.edge == Edge::Right)
        .unwrap()
        .id;
    w.feed(
        1,
        Input::Capture(CaptureEvent::EdgeReleased {
            portal,
            at: ms(w.now),
        }),
    );
    w.crossing(1, 2);
    assert!(!w.trace.iter().any(|(n, o)| *n == 1
        && matches!(
            o,
            Output::SendControl {
                msg: ControlMessage::ClipOffer(_),
                ..
            }
        )));
    assert!(w.hosts[2].current_promise().is_none());
    assert_eq!(w.hosts[1].reads, 0);
}
#[test]
fn oversized_holder_read_is_refused_without_truncation() {
    let mut w = World::new(2);
    w.copy(0, &vec![b'x'; MAX_CLIP_TEXT as usize + 1]);
    w.crossing(0, 1);
    w.paste(1, 50);
    assert_eq!(w.answer(1, 50), Some(&None));
    assert!(w.trace.iter().any(|(_, o)| matches!(
        o,
        Output::SendControl {
            msg: ControlMessage::ClipFetchFailed(ClipFetchFailed {
                reason: ClipFailure::TooLarge,
                ..
            }),
            ..
        }
    )));
    assert!(
        !w.trace
            .iter()
            .any(|(_, o)| matches!(o, Output::SendClipData { .. }))
    );
}
#[test]
fn two_serves_per_peer_and_new_capacity_after_completion() {
    let mut w = World::new(2);
    w.copy(0, b"fixture");
    w.crossing(0, 1);
    w.delay_reads = true;
    for id in 60..63 {
        w.paste(1, id);
    }
    assert_eq!(w.hosts[0].reads, 2);
    assert_eq!(w.answer(1, 62), Some(&None));
    w.flush_reads();
    w.paste(1, 63);
    assert_eq!(w.hosts[0].reads, 3);
}
#[test]
fn wrong_peer_kind_fetch_and_over_cap_data_do_not_answer() {
    let mut e = prepared();
    let p = promise(&mut e, 1);
    let f = paste(&mut e, p, 70, 0);
    for input in [
        Input::ClipData {
            peer: NODES[2],
            fetch: f,
            kind: ClipKind::Text,
            data: ClipBytes(vec![]),
        },
        Input::ClipData {
            peer: NODES[0],
            fetch: f,
            kind: ClipKind::Image,
            data: ClipBytes(vec![]),
        },
        data(ClipFetchId(f.0 + 1), b"unknown"),
        data(f, &vec![0; MAX_CLIP_TEXT as usize + 1]),
    ] {
        assert!(only(e.handle(input, ms(0))).is_empty());
    }
    assert!(
        e.handle(data(f, b"valid"), ms(0))
            .iter()
            .any(|o| matches!(o, Output::ClipFulfil { data: Some(d), .. } if d.0 == b"valid"))
    );
    assert!(only(e.handle(data(f, b"duplicate"), ms(0))).is_empty());
}
#[test]
fn promise_lost_and_matching_remote_withdraw_answer_empty() {
    for remote in [false, true] {
        let mut e = prepared();
        let p = promise(&mut e, 1);
        let f = paste(&mut e, p, 71, 0);
        let i = if remote {
            control(
                NODES[0],
                ControlMessage::ClipWithdraw(ClipWithdraw {
                    offer: ClipOfferId(1),
                }),
            )
        } else {
            Input::Clipboard(ClipboardEvent::PromiseLost { offer: p })
        };
        let out = e.handle(i, ms(0));
        assert!(has_withdraw(&out));
        assert!(empty(&out, 71));
        assert!(only(e.handle(data(f, b"late"), ms(0))).is_empty());
    }
}
#[test]
fn feature_removal_and_link_loss_cancel_without_sending_to_unadmitted_peers() {
    for i in [
        Input::ClipPeer {
            peer: NODES[0],
            available: false,
        },
        Input::Link(LinkEvent::Closed {
            peer: NODES[0],
            error: LinkError::Closed,
        }),
    ] {
        let mut e = prepared();
        let p = promise(&mut e, 1);
        let f = paste(&mut e, p, 72, 0);
        let out = only(e.handle(i, ms(0)));
        assert!(has_withdraw(&out));
        assert!(empty(&out, 72));
        assert!(!out.iter().any(|o| matches!(o, Output::SendControl { .. })));
        assert!(only(e.handle(data(f, b"late"), ms(0))).is_empty());
        e.handle(Input::PeerUp { peer: NODES[0] }, ms(0));
        e.handle(
            Input::ClipPeer {
                peer: NODES[0],
                available: true,
            },
            ms(0),
        );
        assert!(only(e.handle(remote_offer(1, vec![ClipKind::Text]), ms(0))).is_empty());
    }
}
#[test]
fn sleep_panic_and_inactive_session_clear_promises_until_a_fresh_state() {
    for event in [
        Input::Session(SessionEvent::WillSleep),
        Input::Command(Command::Panic),
        Input::Session(SessionEvent::State(SessionState {
            lock: LockState::Unlocked,
            active: None,
        })),
    ] {
        let mut e = prepared();
        let p = promise(&mut e, 1);
        paste(&mut e, p, 73, 0);
        let out = e.handle(event, ms(0));
        assert!(has_withdraw(&out));
        assert!(empty(&out, 73));
        assert!(only(e.handle(remote_offer(2, vec![ClipKind::Text]), ms(0))).is_empty());
    }
}
#[test]
fn debug_never_formats_clipboard_content() {
    let marker = "private-fixture-marker";
    let d = ClipBytes(marker.as_bytes().to_vec());
    assert_eq!(format!("{d:?}"), "ClipBytes(22 bytes)");
    let values = [
        format!("{:?}", data(ClipFetchId(1), marker.as_bytes())),
        format!(
            "{:?}",
            Input::ClipReadDone {
                peer: NODES[0],
                fetch: ClipFetchId(1),
                result: Ok(d.clone())
            }
        ),
        format!(
            "{:?}",
            Output::SendClipData {
                peer: NODES[0],
                fetch: ClipFetchId(1),
                kind: ClipKind::Text,
                data: d.clone()
            }
        ),
        format!(
            "{:?}",
            Output::ClipFulfil {
                paste: crosspane_platform::LocalPasteId(1),
                data: Some(d)
            }
        ),
    ];
    for value in values {
        assert!(!value.contains(marker));
        assert!(!value.contains("112, 114, 105"));
    }
}

fn current_offer(w: &World, node: usize) -> ClipOfferId {
    w.trace
        .iter()
        .rev()
        .find_map(|(n, o)| match o {
            Output::SendControl {
                msg: ControlMessage::ClipOffer(o),
                ..
            } if *n == node => Some(o.offer),
            _ => None,
        })
        .unwrap()
}
fn fetch(offer: ClipOfferId, id: u64, kind: ClipKind) -> Input {
    control(
        NODES[1],
        ControlMessage::ClipFetch(ClipFetch {
            fetch: ClipFetchId(id),
            offer,
            kind,
        }),
    )
}
fn failure(out: Vec<Output>, expected: ClipFailure) {
    let out = only(out);
    assert_eq!(out.len(), 1);
    assert!(
        matches!(&out[0], Output::SendControl { msg: ControlMessage::ClipFetchFailed(f), .. } if f.reason == expected)
    );
}
#[test]
fn fetch_failure_order_is_grant_then_lock_then_offer_then_capacity() {
    let mut w = World::new(2);
    w.copy(0, b"fixture");
    w.crossing(0, 1);
    let offer = current_offer(&w, 0);
    let e = &mut w.engines[0];
    e.handle(grant(false, true), ms(0));
    e.handle(state(LockState::Locked), ms(0));
    failure(
        e.handle(fetch(ClipOfferId(999), 1, ClipKind::Image), ms(0)),
        ClipFailure::NotGranted,
    );
    e.handle(grant(true, true), ms(0));
    failure(
        e.handle(fetch(ClipOfferId(999), 2, ClipKind::Image), ms(0)),
        ClipFailure::Locked,
    );
    e.handle(open(), ms(0));
    failure(
        e.handle(fetch(offer, 3, ClipKind::Text), ms(0)),
        ClipFailure::Expired,
    );
    let mut w = World::new(2);
    w.copy(0, b"fixture");
    w.crossing(0, 1);
    let offer = current_offer(&w, 0);
    let e = &mut w.engines[0];
    for id in 1..=2 {
        assert!(
            matches!(&only(e.handle(fetch(offer, id, ClipKind::Text), ms(0)))[0], Output::ClipRead { max_bytes, .. } if *max_bytes == MAX_CLIP_TEXT as usize)
        );
    }
    failure(
        e.handle(fetch(offer, 3, ClipKind::Image), ms(0)),
        ClipFailure::Expired,
    );
    failure(
        e.handle(fetch(offer, 3, ClipKind::Text), ms(0)),
        ClipFailure::Unavailable,
    );
    failure(
        e.handle(fetch(offer, 1, ClipKind::Text), ms(0)),
        ClipFailure::Unavailable,
    );
}
#[test]
fn changed_during_read_rechecks_offer_before_sending_content() {
    let mut w = World::new(2);
    w.copy(0, b"old fixture");
    w.crossing(0, 1);
    w.delay_reads = true;
    w.paste(1, 80);
    w.copy(0, b"new fixture");
    assert_eq!(w.answer(1, 80), Some(&None));
    w.flush_reads();
    assert!(w.trace.iter().any(|(_, o)| matches!(
        o,
        Output::SendControl {
            msg: ControlMessage::ClipFetchFailed(ClipFetchFailed {
                reason: ClipFailure::Expired,
                ..
            }),
            ..
        }
    )));
    assert!(
        !w.trace
            .iter()
            .any(|(_, o)| matches!(o, Output::SendClipData { .. }))
    );
}
#[test]
fn read_completion_rejects_over_cap_and_passes_backend_errors() {
    for result in [
        Ok(ClipBytes(vec![0; MAX_CLIP_TEXT as usize + 1])),
        Err(ClipFailure::Unavailable),
    ] {
        let expected = if result.is_ok() {
            ClipFailure::TooLarge
        } else {
            ClipFailure::Unavailable
        };
        let mut w = World::new(2);
        w.copy(0, b"fixture");
        w.crossing(0, 1);
        let offer = current_offer(&w, 0);
        w.engines[0].handle(fetch(offer, 99, ClipKind::Text), ms(0));
        failure(
            w.engines[0].handle(
                Input::ClipReadDone {
                    peer: NODES[1],
                    fetch: ClipFetchId(99),
                    result,
                },
                ms(0),
            ),
            expected,
        );
        assert!(
            only(w.engines[0].handle(
                Input::ClipReadDone {
                    peer: NODES[1],
                    fetch: ClipFetchId(99),
                    result: Ok(ClipBytes(vec![]))
                },
                ms(0)
            ))
            .is_empty()
        );
    }
}
#[test]
fn image_metadata_uses_image_cap_and_preserves_kind() {
    let mut w = World::new(2);
    w.hosts[0].copy(None, Some(vec![1, 2, 3]));
    w.pump();
    w.crossing(0, 1);
    assert_eq!(
        w.hosts[1].current_promise().unwrap().1,
        ClipKinds {
            text: false,
            image: true
        }
    );
    w.hosts[1].paste(crosspane_platform::LocalPasteId(90), ClipKind::Image);
    w.pump();
    assert_eq!(w.answer(1, 90), Some(&Some(vec![1, 2, 3])));
    assert!(w.trace.iter().any(|(_, o)| matches!(o, Output::ClipRead { kind: ClipKind::Image, max_bytes, .. } if *max_bytes == MAX_CLIP_IMAGE as usize)));
    assert!(w.trace.iter().any(|(_, o)| matches!(
        o,
        Output::SendClipData {
            kind: ClipKind::Image,
            ..
        }
    )));
}
#[test]
fn accepted_remote_promise_invalidates_native_offers_and_pending_reads() {
    let mut w = World::new(2);
    w.copy(0, b"A fixture");
    w.crossing(0, 1);
    w.delay_reads = true;
    w.paste(1, 91);
    w.copy(1, b"B fixture");
    assert!(w.hosts[0].current_promise().is_some());
    assert_eq!(w.answer(1, 91), Some(&None));
    w.flush_reads();
    assert!(
        !w.trace
            .iter()
            .any(|(_, o)| matches!(o, Output::SendClipData { .. }))
    );
}
#[test]
fn fresh_change_restores_native_kinds_after_peer_promise() {
    let mut w = World::new(2);
    w.copy(0, b"A fixture");
    w.crossing(0, 1);
    w.copy(1, b"B fixture");
    assert!(w.hosts[0].current_promise().is_some());
    w.copy(0, b"new native A");
    w.paste(1, 92);
    assert_eq!(w.answer(1, 92), Some(&Some(b"new native A".to_vec())));
}
#[test]
fn mismatched_withdraw_failure_and_paste_kind_do_not_touch_current_promise() {
    let mut e = prepared();
    let p = promise(&mut e, 1);
    let f = paste(&mut e, p, 93, 0);
    for input in [
        control(
            NODES[0],
            ControlMessage::ClipWithdraw(ClipWithdraw {
                offer: ClipOfferId(2),
            }),
        ),
        control(
            NODES[2],
            ControlMessage::ClipFetchFailed(ClipFetchFailed {
                fetch: f,
                reason: ClipFailure::Expired,
            }),
        ),
        Input::Clipboard(ClipboardEvent::PromiseLost { offer: p + 1 }),
    ] {
        assert!(only(e.handle(input, ms(0))).is_empty());
    }
    let out = e.handle(
        Input::Clipboard(ClipboardEvent::PasteRequested {
            paste: crosspane_platform::LocalPasteId(94),
            offer: p,
            kind: ClipKind::Image,
        }),
        ms(0),
    );
    assert!(empty(&out, 94));
    assert!(empty(
        &e.handle(
            control(
                NODES[0],
                ControlMessage::ClipFetchFailed(ClipFetchFailed {
                    fetch: f,
                    reason: ClipFailure::Unavailable
                })
            ),
            ms(0)
        ),
        93
    ));
}
#[test]
fn changed_without_accepted_e1_role_never_sends_an_offer() {
    let mut e = prepared();
    assert!(
        only(e.handle(
            Input::Clipboard(ClipboardEvent::Changed { kinds: TEXT }),
            ms(0)
        ))
        .is_empty()
    );
    assert!(only(e.handle(Input::Tick, ms(5000))).is_empty());
}
#[test]
fn feature_before_peer_up_is_not_admission() {
    let mut e = engine(NODES[1]);
    e.handle(grant(true, true), ms(0));
    e.handle(
        Input::ClipPeer {
            peer: NODES[0],
            available: true,
        },
        ms(0),
    );
    e.handle(Input::PeerUp { peer: NODES[0] }, ms(0));
    assert!(only(e.handle(remote_offer(1, vec![ClipKind::Text]), ms(0))).is_empty());
    e.handle(
        Input::ClipPeer {
            peer: NODES[0],
            available: true,
        },
        ms(0),
    );
    assert_eq!(promise(&mut e, 1), 1);
}

#[test]
fn promise_and_fetch_counters_are_fresh_across_supersession() {
    let mut e = prepared();
    let p1 = promise(&mut e, 1);
    let f1 = paste(&mut e, p1, 100, 0);
    let p2 = promise(&mut e, 2);
    let f2 = paste(&mut e, p2, 101, 0);
    assert!(p2 > p1);
    assert!(f2.0 > f1.0);
    assert!(only(e.handle(data(f1, b"stale"), ms(0))).is_empty());
}
#[test]
fn fake_host_never_emits_changed_for_its_own_promise_and_ignores_duplicate_answers() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let q = Arc::clone(&events);
    let mut host = FakeClipboardHost::default();
    host.subscribe(Arc::new(move |e| q.lock().unwrap().push(e)))
        .unwrap();
    host.copy(Some(b"fixture".to_vec()), None);
    assert_eq!(host.kinds().unwrap(), TEXT);
    assert_eq!(host.reads, 0);
    events.lock().unwrap().clear();
    host.promise(1, TEXT).unwrap();
    assert!(events.lock().unwrap().is_empty());
    let id = crosspane_platform::LocalPasteId(1);
    host.paste(id, ClipKind::Text);
    host.withdraw(1).unwrap();
    host.fulfil(id, Some(b"late".to_vec()));
    assert_eq!(host.answer(id), Some(&None));
    host.fulfil(
        crosspane_platform::LocalPasteId(999),
        Some(b"unknown".to_vec()),
    );
    assert!(host.answer(crosspane_platform::LocalPasteId(999)).is_none());
    assert!(!format!("{host:?}").contains("fixture"));
}

#[test]
fn controller_offer_waits_for_accepted_capture_and_handshake() {
    let mut w = World::new(2);
    w.copy(0, b"fixture");
    let portal = w.portals[0]
        .iter()
        .find(|p| p.edge == Edge::Right)
        .unwrap()
        .id;
    let e = &mut w.engines[0];
    let out = e.handle(
        Input::Capture(CaptureEvent::EdgePressed {
            portal,
            position: 0.5,
            at: ms(0),
        }),
        ms(0),
    );
    assert!(
        out.iter().any(
            |o| matches!(o, Output::ShowOverlay { id, .. } if *id == crosspane_engine::io::HUD)
        )
    );
    assert!(only(out).is_empty());
    let out = e.handle(
        Input::Overlay(OverlayEvent::Visible(crosspane_engine::io::HUD)),
        ms(0),
    );
    let session = out
        .iter()
        .find_map(|o| match o {
            Output::SendControl {
                msg: ControlMessage::StartControl { session, .. },
                ..
            } => Some(*session),
            _ => None,
        })
        .unwrap();
    assert!(only(out).is_empty());
    assert_eq!(e.control_established(), None);
    let out = e.handle(
        control(NODES[1], ControlMessage::ControlStarted { session }),
        ms(0),
    );
    let capture = out
        .iter()
        .find_map(|o| match o {
            Output::BeginCapture { id, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    assert!(only(out).is_empty());
    assert!(
        only(e.handle(Input::Capture(CaptureEvent::Started { id: capture }), ms(0))).is_empty()
    );
    let out = e.handle(
        Input::CaptureBegun {
            id: capture,
            result: Ok(CaptureStart {
                held_keys: vec![],
                lock_keys: LockKeys::default(),
            }),
        },
        ms(0),
    );
    assert_eq!(e.control_established(), Some(NODES[1]));
    assert!(out.iter().any(|o| matches!(
        o,
        Output::SendControl {
            msg: ControlMessage::ClipOffer(_),
            ..
        }
    )));
}

#[test]
fn valid_offer_seen_while_locked_or_ungranted_cannot_replay_after_reopening() {
    for blocked in [state(LockState::Locked), grant(true, false)] {
        let mut e = prepared();
        e.handle(blocked, ms(0));
        assert!(only(e.handle(remote_offer(10, vec![ClipKind::Text]), ms(0))).is_empty());
        e.handle(open(), ms(0));
        e.handle(grant(true, true), ms(0));
        assert!(only(e.handle(remote_offer(10, vec![ClipKind::Text]), ms(0))).is_empty());
        assert_eq!(promise(&mut e, 11), 1);
    }
}

#[test]
fn cancelled_fetch_id_cannot_admit_reuse_or_misattribute_a_late_old_read() {
    let mut w = World::new(2);
    w.copy(0, b"old fixture");
    w.crossing(0, 1);
    w.delay_reads = true;
    w.paste(1, 200);
    let (_, old_completion) = w.delayed.pop().unwrap();
    let Input::ClipReadDone { fetch: old_id, .. } = &old_completion else {
        panic!("expected delayed read");
    };
    let old_id = *old_id;
    w.feed(0, grant(false, true));
    w.feed(0, grant(true, true));
    w.copy(0, b"new fixture");
    let offer = current_offer(&w, 0);
    let e = &mut w.engines[0];
    let reused = e.handle(fetch(offer, old_id.0, ClipKind::Text), ms(0));
    let fresh = ClipFetchId(old_id.0 + 1);
    assert!(matches!(
        &only(e.handle(fetch(offer, fresh.0, ClipKind::Text), ms(0)))[0],
        Output::ClipRead { fetch, .. } if *fetch == fresh
    ));
    assert!(only(e.handle(old_completion, ms(0))).is_empty());
    failure(reused, ClipFailure::Unavailable);
    let out = only(e.handle(
        Input::ClipReadDone {
            peer: NODES[1],
            fetch: fresh,
            result: Ok(ClipBytes(b"new fixture".to_vec())),
        },
        ms(0),
    ));
    assert!(matches!(
        &out[0], Output::SendClipData { fetch, data, .. }
        if *fetch == fresh && data.0 == b"new fixture"
    ));
}

#[test]
fn completed_fetch_watermark_rejects_older_ids_without_another_read() {
    let mut w = World::new(2);
    w.copy(0, b"fixture");
    w.crossing(0, 1);
    let offer = current_offer(&w, 0);
    let e = &mut w.engines[0];
    assert!(matches!(
        &only(e.handle(fetch(offer, 100, ClipKind::Text), ms(0)))[0],
        Output::ClipRead { .. }
    ));
    e.handle(
        Input::ClipReadDone {
            peer: NODES[1],
            fetch: ClipFetchId(100),
            result: Err(ClipFailure::Unavailable),
        },
        ms(0),
    );
    for id in [99, 100] {
        failure(
            e.handle(fetch(offer, id, ClipKind::Text), ms(0)),
            ClipFailure::Unavailable,
        );
    }
}

#[test]
fn gate_closure_between_fetch_admission_and_execution_returns_locked_without_reading() {
    let mut w = World::new(2);
    w.copy(0, b"fixture");
    w.crossing(0, 1);
    let offer = current_offer(&w, 0);
    let read = only(w.engines[0].handle(fetch(offer, 100, ClipKind::Text), ms(0)))
        .pop()
        .unwrap();
    assert!(matches!(read, Output::ClipRead { .. }));
    w.gates[0].set_session_permits(false);
    w.trace.clear();
    w.execute(0, read);
    w.pump();
    assert_eq!(w.hosts[0].reads, 0);
    assert!(w.trace.iter().any(|(n, o)| *n == 0
        && matches!(
            o,
            Output::SendControl {
                msg: ControlMessage::ClipFetchFailed(ClipFetchFailed {
                    reason: ClipFailure::Locked,
                    ..
                }),
                ..
            }
        )));
    assert!(
        !w.trace
            .iter()
            .any(|(_, o)| matches!(o, Output::SendClipData { .. }))
    );
}

#[test]
fn gate_closure_between_promise_admission_and_execution_returns_locked() {
    let mut w = World::new(2);
    let promise = only(w.engines[1].handle(remote_offer(1, vec![ClipKind::Text]), ms(0)))
        .pop()
        .unwrap();
    let Output::ClipPromise { offer, kinds } = promise else {
        panic!("expected admitted promise");
    };
    w.gates[1].set_engine_permits(false);
    assert!(matches!(
        w.hosts[1].promise(offer, kinds),
        Err(crosspane_platform::PlatformError::Locked)
    ));
    assert!(w.hosts[1].current_promise().is_none());
}

#[test]
fn promise_replaces_native_content_and_withdrawal_never_resurrects_it() {
    let mut host = FakeClipboardHost::default();
    host.copy(Some(b"old fixture".to_vec()), Some(vec![1, 2, 3]));
    host.promise(1, TEXT).unwrap();
    host.withdraw(1).unwrap();
    assert_eq!(host.kinds().unwrap(), ClipKinds::default());
    for kind in [ClipKind::Text, ClipKind::Image] {
        assert!(matches!(
            host.read(kind, 100),
            Err(crosspane_platform::PlatformError::NotFound)
        ));
    }
}

#[test]
fn fake_missing_kind_returns_not_found() {
    let mut host = FakeClipboardHost::default();
    for kind in [ClipKind::Text, ClipKind::Image] {
        assert!(matches!(
            host.read(kind, 100),
            Err(crosspane_platform::PlatformError::NotFound)
        ));
    }
}

#[test]
fn fake_own_promise_returns_not_found() {
    let mut host = FakeClipboardHost::default();
    host.copy(Some(b"fixture".to_vec()), None);
    host.promise(1, TEXT).unwrap();
    assert!(matches!(
        host.read(ClipKind::Text, 100),
        Err(crosspane_platform::PlatformError::NotFound)
    ));
}

#[test]
fn fake_zero_byte_clipboard_returns_not_found_for_each_kind() {
    let mut host = FakeClipboardHost::default();
    host.copy(Some(vec![]), Some(vec![]));
    for kind in [ClipKind::Text, ClipKind::Image] {
        assert!(matches!(
            host.read(kind, 100),
            Err(crosspane_platform::PlatformError::NotFound)
        ));
    }
}

#[test]
fn fake_invalid_utf8_text_returns_not_found() {
    let mut host = FakeClipboardHost::default();
    host.copy(Some(vec![0xff]), None);
    assert!(matches!(
        host.read(ClipKind::Text, 100),
        Err(crosspane_platform::PlatformError::NotFound)
    ));
}

#[test]
fn fake_empty_promise_kinds_returns_exact_backend_failure() {
    let mut host = FakeClipboardHost::default();
    assert!(matches!(
        host.promise(1, ClipKinds::default()),
        Err(crosspane_platform::PlatformError::Backend(message))
        if message == "empty clipboard promise"
    ));
    assert!(host.current_promise().is_none());
}
