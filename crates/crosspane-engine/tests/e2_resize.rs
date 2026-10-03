//! WP-2.34: resize authority between the E2 destination and source roles. Request numbers
//! (`Resize.request`, `Geometry.answers`) decide which source reply may resize a proxy; the
//! destination's classification of host callbacks only saves round trips and must fail safe.
//!
//! All time is explicit: nothing here sleeps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;

use proptest::prelude::*;

use crosspane_engine::e2::E2;
use crosspane_engine::{Command, EngineConfig, Input, Output, ProjectionKey, ProxyEvent};
use crosspane_input::journal::MemoryJournal;
use crosspane_platform::{
    LockState, Parked, ParkingKind as PlatformParking, SessionEvent, SessionState, StreamId,
    WindowEvent, WindowInfo, WindowRole, WindowState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ControlMessage};
use crosspane_protocol::projection::{ParkingKind, ProjectionMessage as Message, WindowSummary};
use crosspane_types::geom::{PixelRect, PixelSize, PointLogical, RectLogical, SizeLogical};
use crosspane_types::id::{DisplayId, NodeId, ProjectionId, WindowId};
use crosspane_types::time::MonoTime;

/// The node that owns the window.
const SRC: NodeId = NodeId([1; 32]);
/// The node that shows the proxy.
const DST: NodeId = NodeId([2; 32]);
const ID: ProjectionId = ProjectionId(1);
const WINDOW: WindowId = WindowId(10);
const DISPLAY: DisplayId = DisplayId(4);
const OPEN: SessionState = SessionState {
    lock: LockState::Unlocked,
    active: Some(true),
};
/// The proxy's scale throughout, except in the scale tests.
const SCALE: f64 = 2.0;

fn ms(n: u64) -> MonoTime {
    MonoTime::from_nanos(n * 1_000_000)
}
fn px(width: u32, height: u32) -> PixelSize {
    PixelSize::new(width, height)
}
/// The size the window and its proxy start with.
fn open_size() -> PixelSize {
    px(640, 480)
}
fn proxy_key() -> ProjectionKey {
    ProjectionKey {
        source: SRC,
        projection: ID,
    }
}
fn control(peer: NodeId, msg: Message) -> Input {
    Input::Link(LinkEvent::Control {
        peer,
        msg: ControlMessage::Projection(msg),
    })
}
fn resize(request: u32, size: PixelSize, scale: f64) -> Message {
    Message::Resize {
        fullscreen: false,
        projection: ID,
        request,
        size,
        scale,
    }
}
fn geometry(size: PixelSize, answers: u32) -> Message {
    Message::Geometry {
        fullscreen: None,
        projection: ID,
        size,
        parking: ParkingKind::Twin,
        answers,
    }
}
fn actual_geometry(size: PixelSize, answers: u32) -> Message {
    let mut message = geometry(size, answers);
    if let Message::Geometry { fullscreen, .. } = &mut message {
        *fullscreen = Some(false);
    }
    message
}
fn sent(peer: NodeId, msg: Message) -> Output {
    Output::SendControl {
        peer,
        msg: ControlMessage::Projection(msg),
    }
}
fn messages(out: &[Output]) -> Vec<Message> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendControl {
                msg: ControlMessage::Projection(m),
                ..
            } => Some(m.clone()),
            _ => None,
        })
        .collect()
}
fn proxy_geometries(out: &[Output]) -> Vec<PixelSize> {
    out.iter()
        .filter_map(|o| match o {
            Output::ProxyGeometry { size, .. } => Some(*size),
            _ => None,
        })
        .collect()
}
/// Everything but input heartbeats: what a resize exchange can produce.
fn traffic(out: &[Output]) -> Vec<Output> {
    out.iter()
        .filter(|o| !matches!(o, Output::SendInput { .. }))
        .cloned()
        .collect()
}
fn window() -> WindowInfo {
    WindowInfo {
        id: WINDOW,
        title: "fixture".into(),
        app_id: "test".into(),
        pid: None,
        display: Some(DISPLAY),
        frame: RectLogical::new(PointLogical::zero(), SizeLogical::new(320.25, 240.25)),
        state: WindowState::Normal,
        role: WindowRole::Toplevel,
        parent: None,
    }
}
fn parked(size: PixelSize) -> Parked {
    Parked {
        fullscreen: false,
        window: WINDOW,
        kind: PlatformParking::Twin,
        display: DISPLAY,
        content: PixelRect::new(
            crosspane_types::geom::euclid::Point2D::new(20, 30),
            crosspane_types::geom::euclid::Point2D::new(
                20 + size.width as i32,
                30 + size.height as i32,
            ),
        ),
    }
}
fn node(node: NodeId, peer: NodeId) -> E2 {
    let (mut e2, out) = E2::new(
        &EngineConfig::new(node),
        Box::new(MemoryJournal::default()),
        ms(0),
    )
    .unwrap();
    assert!(out.is_empty());
    let mut sink = Vec::new();
    e2.handle(&Input::Session(SessionEvent::State(OPEN)), ms(0), &mut sink);
    e2.handle(&Input::PeerUp { peer }, ms(0), &mut sink);
    e2.handle(
        &Input::Grants(
            [(
                peer,
                [Capability::WindowShare, Capability::WindowPresent].into(),
            )]
            .into(),
        ),
        ms(0),
        &mut sink,
    );
    e2
}

// ---- The destination alone ----

/// A destination with its proxy open at `open_size()`, scale `SCALE`, and the source's first
/// geometry (the window at that size) already applied. Time never goes backwards.
struct Dst {
    e2: E2,
    last: u64,
}

impl Dst {
    fn new() -> Self {
        let mut dst = Self {
            e2: node(DST, SRC),
            last: 0,
        };
        dst.at(
            control(
                SRC,
                Message::Start {
                    projection: ID,
                    window: WindowSummary {
                        title: "fixture".into(),
                        app_id: "test".into(),
                    },
                    size: open_size(),
                },
            ),
            0,
        );
        let accepted = dst.at(
            Input::ProxyOpened {
                key: proxy_key(),
                result: Ok((open_size(), SCALE)),
            },
            0,
        );
        assert_eq!(
            messages(&accepted),
            vec![Message::Accepted {
                projection: ID,
                size: open_size(),
                scale: SCALE
            }]
        );
        // The first geometry tells the host the parking kind, even at the proxy's own size.
        assert_eq!(
            geometries(&dst.geometry(open_size(), 0, 0)),
            vec![(open_size(), ParkingKind::Twin)]
        );
        dst
    }
    fn at(&mut self, input: Input, now: u64) -> Vec<Output> {
        assert!(
            now >= self.last,
            "time went backwards: {now} < {}",
            self.last
        );
        self.last = now;
        let mut out = Vec::new();
        self.e2.handle(&input, ms(now), &mut out);
        out
    }
    /// The proxy's content area changed (the user, or the host's own resize completing).
    fn resized(&mut self, size: PixelSize, scale: f64, now: u64) -> Vec<Output> {
        self.at(
            Input::Proxy {
                key: proxy_key(),
                event: ProxyEvent::Resized { size, scale },
            },
            now,
        )
    }
    /// A `Geometry` from the source.
    fn geometry(&mut self, size: PixelSize, answers: u32, now: u64) -> Vec<Output> {
        self.at(control(SRC, geometry(size, answers)), now)
    }
    fn tick(&mut self, now: u64) -> Vec<Output> {
        self.at(Input::Tick, now)
    }
    /// Tick as the agent does, at every deadline up to and including `to` (ms), from `from` on.
    /// Returns what each tick produced, minus heartbeats.
    fn run(&mut self, from: u64, to: u64) -> Vec<(u64, Output)> {
        let mut seen = Vec::new();
        let mut now = from;
        for _ in 0..10_000 {
            let Some(deadline) = self.e2.next_deadline() else {
                break;
            };
            let at = (deadline.as_nanos() / 1_000_000).max(now);
            if at > to {
                return seen;
            }
            now = at;
            seen.extend(traffic(&self.tick(at)).into_iter().map(|o| (at, o)));
        }
        panic!("the deadline never advanced");
    }
    /// Tick every `step` ms in `(from, to]`, whatever the deadline says.
    fn run_every(&mut self, step: u64, from: u64, to: u64) -> Vec<(u64, Output)> {
        let mut seen = Vec::new();
        let mut at = from + step;
        while at <= to {
            seen.extend(traffic(&self.tick(at)).into_iter().map(|o| (at, o)));
            at += step;
        }
        seen
    }
    /// Link drop and resume, as the agent drives them.
    fn drop_link(&mut self, now: u64) {
        self.at(
            Input::Link(LinkEvent::Closed {
                peer: SRC,
                error: LinkError::Closed,
            }),
            now,
        );
    }
    fn resume(&mut self, now: u64) -> Vec<Output> {
        self.at(Input::PeerUp { peer: SRC }, now);
        self.at(
            control(
                SRC,
                Message::Start {
                    projection: ID,
                    window: WindowSummary {
                        title: "fixture".into(),
                        app_id: "test".into(),
                    },
                    size: open_size(),
                },
            ),
            now,
        )
    }
}

fn geometries(out: &[Output]) -> Vec<(PixelSize, ParkingKind)> {
    out.iter()
        .filter_map(|o| match o {
            Output::ProxyGeometry { size, parking, .. } => Some((*size, *parking)),
            _ => None,
        })
        .collect()
}
fn proxy_geometries_of(seen: &[(u64, Output)]) -> Vec<PixelSize> {
    proxy_geometries(&seen.iter().map(|(_, o)| o.clone()).collect::<Vec<_>>())
}
fn messages_of(seen: &[(u64, Output)]) -> Vec<Message> {
    messages(&seen.iter().map(|(_, o)| o.clone()).collect::<Vec<_>>())
}

#[test]
fn queued_b_survives_a_stale_answer() {
    let mut dst = Dst::new();
    let a = px(700, 500);
    let b = px(720, 520);
    assert_eq!(
        messages(&dst.resized(a, SCALE, 0)),
        vec![resize(1, a, SCALE)]
    );
    // Within the slot: B is held back for the slot, not sent.
    assert!(dst.resized(b, SCALE, 10).is_empty());
    // The source answers A (request 1, which is the newest request sent so far). The user is
    // still resizing, and B is queued: the answer must not resize the proxy back to A.
    assert!(dst.geometry(a, 1, 20).is_empty());
    let out = dst.tick(50);
    assert_eq!(traffic(&out), vec![sent(SRC, resize(2, b, SCALE))]);
    // B's own answer arrives; the proxy is already at B. Nothing more, ever.
    assert!(traffic(&dst.geometry(b, 2, 100)).is_empty());
    assert!(dst.run(100, 5_000).is_empty());
}

#[test]
fn late_final_answer_resizes_the_proxy_once_and_only_then() {
    for (final_size, expect) in [(px(760, 540), true), (px(720, 520), false)] {
        let mut dst = Dst::new();
        let a = px(700, 500);
        let b = px(720, 520);
        assert_eq!(
            messages(&dst.resized(a, SCALE, 0)),
            vec![resize(1, a, SCALE)]
        );
        assert_eq!(
            messages(&dst.resized(b, SCALE, 50)),
            vec![resize(2, b, SCALE)]
        );
        // The source's answer to the older request: ignored, however long it all takes.
        assert!(dst.geometry(a, 1, 100).is_empty());
        // The user is quiet from 300 ms; the final answer takes 3 s.
        let before = dst.run_every(100, 100, 2_900);
        assert!(proxy_geometries_of(&before).is_empty(), "{before:?}");
        assert!(messages_of(&before).is_empty());
        let answer = dst.geometry(final_size, 2, 3_000);
        // The user has been quiet for a long time: applied at once, once.
        assert_eq!(
            proxy_geometries(&answer),
            if expect { vec![final_size] } else { vec![] }
        );
        // Its callback is the host's own; the ticks go on to 5 s with nothing more.
        assert!(dst.resized(final_size, SCALE, 3_050).is_empty());
        let after = dst.run_every(100, 3_050, 5_000);
        assert!(after.is_empty(), "{after:?}");
    }
}

#[test]
fn a_stale_payload_for_a_superseded_command_is_user_intent_and_asks_for_it() {
    // A host that doesn't sample the native size (against R2) can report a stale snapshot. The
    // engine fails safe: it asks the source for the size it is told. (Convergence with hosts that
    // do sample is proved with `World`, below.)
    let (a, b) = (px(700, 500), px(720, 520));
    let mut dst = Dst::new();
    // Two source-initiated geometries, no request outstanding: A then B.
    assert_eq!(proxy_geometries(&dst.geometry(a, 0, 0)), vec![a]);
    assert_eq!(proxy_geometries(&dst.geometry(b, 0, 10)), vec![b]);
    // Callbacks A then B arrive late: A is superseded (user intent, request 1), then B too.
    assert_eq!(
        messages(&dst.resized(a, SCALE, 100)),
        vec![resize(1, a, SCALE)]
    );
    assert_eq!(
        messages(&dst.resized(b, SCALE, 200)),
        vec![resize(2, b, SCALE)]
    );
    // With answers for both, the state converges to B and there is no more traffic.
    assert!(dst.geometry(a, 1, 220).is_empty());
    assert!(dst.geometry(b, 2, 230).is_empty());
    assert!(dst.run(230, 5_000).is_empty());
    // Had only B's callback arrived, nothing would have been sent, and a later user resize is a
    // request.
    let mut dst = Dst::new();
    dst.geometry(a, 0, 0);
    dst.geometry(b, 0, 10);
    assert!(dst.resized(b, SCALE, 100).is_empty());
    let c = px(800, 600);
    assert_eq!(
        messages(&dst.resized(c, SCALE, 6_000)),
        vec![resize(1, c, SCALE)]
    );
}

#[test]
fn a_drag_onto_a_superseded_command_is_a_request() {
    // The swallowed-intent trace: source geometries A then B (answers 0), commands A then B,
    // B's callback, then a genuine drag to A inside A's original TTL: A is not the newest
    // command, so it is user intent.
    let (a, b) = (px(700, 500), px(720, 520));
    let mut dst = Dst::new();
    dst.geometry(a, 0, 0);
    dst.geometry(b, 0, 10);
    assert!(dst.resized(b, SCALE, 100).is_empty());
    assert_eq!(
        messages(&dst.resized(a, SCALE, 500)),
        vec![resize(1, a, SCALE)]
    );
}

#[test]
fn an_old_source_never_resizes_the_proxy_once_requests_were_sent() {
    let mut dst = Dst::new();
    // Before any request, its geometry (answers 0 == no request yet) is current, as before.
    assert_eq!(
        proxy_geometries(&dst.geometry(px(700, 500), 0, 0)),
        vec![px(700, 500)]
    );
    let a = px(800, 600);
    assert_eq!(
        messages(&dst.resized(a, SCALE, 1_000)),
        vec![resize(1, a, SCALE)]
    );
    // It never numbers its answers: they reflect no request. Not now, and not after a long wait.
    assert!(traffic(&dst.geometry(px(900, 700), 0, 1_010)).is_empty());
    assert!(dst.run(1_010, 5_000).is_empty());
    assert!(traffic(&dst.geometry(px(900, 700), 0, 6_000)).is_empty());
    assert!(dst.run(6_000, 9_000).is_empty());
}

#[test]
fn a_repeat_of_the_last_request_is_dropped_only_while_outstanding_or_once_met() {
    let a = px(700, 500);
    let other = px(710, 500);
    let constrained = px(720, 520);
    // Outstanding: the user drifts away within the slot and back to A before any answer.
    let mut dst = Dst::new();
    assert_eq!(
        messages(&dst.resized(a, SCALE, 0)),
        vec![resize(1, a, SCALE)]
    );
    assert!(dst.resized(other, SCALE, 10).is_empty());
    assert!(dst.resized(a, SCALE, 20).is_empty());
    assert!(dst.run(20, 1_000).is_empty());
    // Met: the source's actual size is A, so going back to A is a repeat as well.
    let mut dst = Dst::new();
    dst.resized(a, SCALE, 0);
    assert!(dst.geometry(a, 1, 5).is_empty());
    assert!(dst.resized(other, SCALE, 10).is_empty());
    assert!(dst.resized(a, SCALE, 20).is_empty());
    assert!(dst.run(20, 1_000).is_empty());
    // Not met: the source is at another size, so the user's size is a genuine request.
    let mut dst = Dst::new();
    dst.resized(a, SCALE, 0);
    assert!(dst.geometry(constrained, 1, 5).is_empty());
    assert!(dst.resized(other, SCALE, 10).is_empty());
    assert!(dst.resized(a, SCALE, 20).is_empty());
    assert_eq!(messages_of(&dst.run(20, 1_000)), vec![resize(2, a, SCALE)]);
}

#[test]
fn a_held_answer_is_dropped_when_a_newer_request_is_sent() {
    let mut dst = Dst::new();
    let (a, b, g) = (px(700, 500), px(720, 520), px(800, 600));
    dst.resized(a, SCALE, 0);
    // The answer to request 1 is held for the quiet period...
    assert!(dst.geometry(g, 1, 10).is_empty());
    // ...but request 2 goes out before then: that answer no longer reflects the newest request,
    // and must never resize the proxy, however long the source takes to answer request 2.
    assert_eq!(
        messages(&dst.resized(b, SCALE, 60)),
        vec![resize(2, b, SCALE)]
    );
    assert!(dst.run(60, 5_000).is_empty());
}

#[test]
fn an_immediately_handled_answer_supersedes_a_held_one_even_if_it_needs_no_command() {
    // A3. The user sends A (request 1); the source's answer B is held for the quiet period.
    let (a, b, c) = (px(700, 500), px(800, 600), px(720, 520));
    let mut dst = Dst::new();
    dst.resized(a, SCALE, 0);
    assert!(dst.geometry(b, 1, 10).is_empty());
    // At 250 ms the user is quiet. Before the tick, a newer answer for the same request arrives:
    // the window is at A after all. It is handled at once, needs no command, and B is gone.
    assert!(traffic(&dst.geometry(a, 1, 250)).is_empty());
    assert!(dst.run(250, 5_000).is_empty());
    // The same when the newer answer does need a command: only it is commanded, never B.
    let mut dst = Dst::new();
    dst.resized(a, SCALE, 0);
    assert!(dst.geometry(b, 1, 10).is_empty());
    assert_eq!(proxy_geometries(&dst.geometry(c, 1, 250)), vec![c]);
    assert!(dst.run(250, 5_000).is_empty());
}

#[test]
fn a_parking_change_at_an_unchanged_size_is_forwarded_without_a_command() {
    // A5.
    let a = px(700, 500);
    let mut dst = Dst::new();
    // The same size and parking again: nothing to say.
    assert!(dst.geometry(open_size(), 0, 5).is_empty());
    // A new size is commanded; its callback confirms it.
    assert_eq!(
        geometries(&dst.geometry(a, 0, 10)),
        vec![(a, ParkingKind::Twin)]
    );
    assert!(dst.resized(a, SCALE, 20).is_empty());
    // The window is parked in place now (mirror): the size is unchanged, but the host is told.
    let out = dst.at(
        control(
            SRC,
            Message::Geometry {
                fullscreen: None,
                projection: ID,
                size: a,
                parking: ParkingKind::Mirror,
                answers: 0,
            },
        ),
        30,
    );
    assert_eq!(geometries(&out), vec![(a, ParkingKind::Mirror)]);
    // No command is outstanding for it: the host's same-size report is ignored as unchanged,
    // and nothing else follows.
    assert!(dst.resized(a, SCALE, 40).is_empty());
    assert!(dst.run(40, 5_000).is_empty());
    // Back to twin at a new size: that command is outstanding until confirmed.
    let b = px(720, 520);
    assert_eq!(
        geometries(&dst.geometry(b, 0, 5_000)),
        vec![(b, ParkingKind::Twin)]
    );
    assert!(dst.resized(b, SCALE, 5_010).is_empty());
}

#[test]
fn an_answer_back_to_the_size_before_an_unconfirmed_host_resize_is_still_sent() {
    // Deviation (b), A4. The source's window goes to A and, before the host reports that, back
    // to the size the proxy had: the proxy was told A, so it must be told O as well. The
    // unconfirmed command counts however old it is.
    for gap in [10, 1_001, 60_000] {
        let mut dst = Dst::new();
        let a = px(700, 500);
        assert_eq!(proxy_geometries(&dst.geometry(a, 0, 0)), vec![a]);
        assert_eq!(
            proxy_geometries(&dst.geometry(open_size(), 0, gap)),
            vec![open_size()]
        );
        // The proxy is at O again, with nothing to wait for: the host's reports are unchanged.
        assert!(dst.resized(open_size(), SCALE, gap + 10).is_empty());
        assert!(dst.run(gap + 10, gap + 5_000).is_empty());
    }
}

#[test]
fn a_completion_reported_twice_is_ignored_the_second_time() {
    let mut dst = Dst::new();
    let c = px(700, 500);
    assert_eq!(proxy_geometries(&dst.geometry(c, 0, 0)), vec![c]);
    // The immediate `Some(actual)` and then the native callback: both the host's own.
    assert!(dst.resized(c, SCALE, 10).is_empty());
    assert!(dst.resized(c, SCALE, 20).is_empty());
    assert!(dst.run(20, 5_000).is_empty());
}

#[test]
fn user_collision_inside_the_ttl_sends_both() {
    let mut dst = Dst::new();
    let a = px(700, 500);
    let b = px(720, 520);
    assert_eq!(proxy_geometries(&dst.geometry(a, 0, 0)), vec![a]);
    // The user drags to B before A's callback (which clears the record), then back to A.
    assert_eq!(
        messages(&dst.resized(b, SCALE, 100)),
        vec![resize(1, b, SCALE)]
    );
    assert_eq!(
        messages(&dst.resized(a, SCALE, 200)),
        vec![resize(2, a, SCALE)]
    );
}

#[test]
fn a_scale_change_at_the_commanded_size_is_user_intent() {
    let mut dst = Dst::new();
    let a = px(700, 500);
    assert_eq!(proxy_geometries(&dst.geometry(a, 0, 0)), vec![a]);
    // The same size at another scale is the proxy moving to a new display, not the host's
    // resize completing: the source must hear it.
    assert_eq!(messages(&dst.resized(a, 1.0, 10)), vec![resize(1, a, 1.0)]);
}

#[test]
fn every_user_event_extends_the_quiet_period() {
    let g = px(800, 600);
    // A coalesced event extends it: A is sent at 0, B is coalesced at 10 (so it is quiet at 260,
    // not 250), and the answer to B arrives while it is still pending.
    let mut dst = Dst::new();
    let a = px(700, 500);
    let b = px(720, 520);
    dst.resized(a, SCALE, 0);
    assert!(dst.resized(b, SCALE, 10).is_empty());
    let sent_b = dst.run(10, 55);
    assert_eq!(messages_of(&sent_b), vec![resize(2, b, SCALE)]);
    assert!(dst.geometry(g, 2, 55).is_empty());
    assert!(dst.run(55, 259).is_empty());
    assert_eq!(proxy_geometries_of(&dst.run(259, 260)), vec![g]);
    // A deduplicated event extends it too: B is coalesced at 10, then the user returns to A
    // (the size already sent) at 20: that drops B, and it is quiet at 270.
    let mut dst = Dst::new();
    dst.resized(a, SCALE, 0);
    assert!(dst.resized(b, SCALE, 10).is_empty());
    assert!(dst.resized(a, SCALE, 20).is_empty());
    assert!(dst.geometry(g, 1, 30).is_empty());
    assert!(dst.run(30, 269).is_empty());
    assert_eq!(proxy_geometries_of(&dst.run(269, 270)), vec![g]);
}

#[test]
fn resume_re_requests_the_current_size_and_numbering_continues() {
    // A1, destination side. The user's request 1 is answered, and one more is held.
    let mut dst = Dst::new();
    let a = px(700, 500);
    let b = px(720, 520);
    assert_eq!(
        messages(&dst.resized(a, SCALE, 0)),
        vec![resize(1, a, SCALE)]
    );
    assert!(dst.geometry(px(800, 600), 1, 10).is_empty());
    dst.drop_link(20);
    // The user keeps resizing while the link is down: only the proxy's size is tracked.
    assert!(dst.resized(b, SCALE, 25).is_empty());
    // Resume: Accepted for the size the proxy has now, then at once a fresh numbered Resize for
    // it. The old held answer is gone.
    let out = dst.resume(40);
    assert_eq!(
        messages(&out),
        vec![
            Message::Accepted {
                projection: ID,
                size: b,
                scale: SCALE
            },
            resize(2, b, SCALE)
        ]
    );
    assert!(dst.run(40, 5_000).is_empty());
    // The answer to the resumed request is current; an older one is not.
    assert!(traffic(&dst.geometry(px(900, 700), 1, 5_000)).is_empty());
    assert_eq!(
        geometries(&dst.geometry(px(900, 700), 2, 5_010)),
        vec![(px(900, 700), ParkingKind::Twin)]
    );
    // Numbering continues: the next one is 3.
    let c = px(800, 700);
    assert_eq!(
        messages(&dst.resized(c, SCALE, 9_000)),
        vec![resize(3, c, SCALE)]
    );
}

#[test]
fn suspend_forgets_an_outstanding_host_resize() {
    // A host resize commanded before the drop is no longer remembered, so its callback (the
    // source was told the proxy's old size) is a request for the new one.
    let mut dst = Dst::new();
    let c = px(800, 600);
    assert_eq!(proxy_geometries(&dst.geometry(c, 0, 0)), vec![c]);
    dst.drop_link(10);
    let out = dst.resume(30);
    assert_eq!(
        messages(&out),
        vec![
            Message::Accepted {
                projection: ID,
                size: open_size(),
                scale: SCALE
            },
            resize(1, open_size(), SCALE)
        ]
    );
    // The late callback: the proxy is at C, the source was told O. A genuine request.
    assert_eq!(
        messages(&dst.resized(c, SCALE, 120)),
        vec![resize(2, c, SCALE)]
    );
}

// ---- Both roles, wired ----

/// A real source and a real destination, with the platform and the proxy host modelled by the
/// test.
///
/// - The platform: a resize on the source takes until `complete_parks`, and the window ends at
///   the size asked for, or at `min` if that is larger. `app_resizes_itself` makes the app
///   resize its own window.
/// - The host: `native` is the proxy window's real size. It changes only by the user's drags
///   (`user_drag`) and by the destination's commands (`ProxyGeometry`, which the window system
///   may adjust by `adjust`). Its callbacks report `native` as sampled when they are handled
///   (the render crate's R2 contract), whenever the test delivers them.
/// - The link: `drop_link` and `resume_link`; nothing crosses while it is down.
///
/// Nothing is ever injected to make an exchange finish: convergence is judged on `native`, the
/// source's `actual` and the last answer the source really sent.
struct World {
    src: E2,
    dst: Dst,
    /// The app's smallest content size on the source.
    min: PixelSize,
    /// The window's actual content size on the source.
    actual: PixelSize,
    actual_fullscreen: bool,
    /// The proxy window's native size.
    native: PixelSize,
    native_fullscreen: bool,
    reported_fullscreen: bool,
    windowed: PixelSize,
    last_answer_state: Option<(u32, bool)>,
    /// A tiled host keeps this size despite geometry commands; `None` preserves the old model.
    keeps: Option<PixelSize>,
    /// What the window system adds to a size it is asked for.
    adjust: (u32, u32),
    /// Parking operations the platform hasn't finished: the size asked for, and when.
    parks: VecDeque<(PixelSize, bool, u64)>,
    /// The size the next parking operation ends at, if the app resized itself.
    self_resize: Option<PixelSize>,
    /// Capture starts the platform hasn't finished, and when.
    captures: Vec<(ProjectionId, u64)>,
    /// When each host resize whose callback hasn't been reported yet was commanded.
    callbacks_due: VecDeque<u64>,
    link_up: bool,
    /// What the destination asked the proxy host for (`ProxyGeometry` that changed its size).
    commands: Vec<PixelSize>,
    /// The `Resize` messages the destination sent: (request, size, scale).
    requests: Vec<(u32, PixelSize, f64)>,
    /// The `Geometry` messages the source sent: (answers, size).
    answers: Vec<(u32, PixelSize)>,
    frames: u32,
    last_self_resize: Option<u64>,
    now: u64,
}

impl World {
    fn new(min: PixelSize) -> Self {
        Self::open_at(min, open_size(), None)
    }

    fn open_at(min: PixelSize, opened: PixelSize, keeps: Option<PixelSize>) -> Self {
        let mut world = Self {
            src: node(SRC, DST),
            dst: Dst {
                e2: node(DST, SRC),
                last: 0,
            },
            min: if keeps.is_some() { min } else { px(1, 1) },
            actual: open_size(),
            actual_fullscreen: false,
            native: opened,
            native_fullscreen: false,
            reported_fullscreen: false,
            windowed: opened,
            last_answer_state: None,
            keeps,
            adjust: (0, 0),
            parks: VecDeque::new(),
            self_resize: None,
            captures: Vec::new(),
            callbacks_due: VecDeque::new(),
            link_up: true,
            commands: Vec::new(),
            requests: Vec::new(),
            answers: Vec::new(),
            frames: 0,
            last_self_resize: None,
            now: 0,
        };
        let mut sink = Vec::new();
        world.src.handle(
            &Input::Windows(WindowEvent::Added(window())),
            ms(0),
            &mut sink,
        );
        world.src.handle(
            &Input::Windows(WindowEvent::Focused(Some(WINDOW))),
            ms(0),
            &mut sink,
        );
        let mut out = Vec::new();
        world.src.handle(
            &Input::Command(Command::Project {
                window: WINDOW,
                to: DST,
                place: None,
            }),
            ms(0),
            &mut out,
        );
        world.pump_src(out, 0);
        let accepted = world.dst.at(
            Input::ProxyOpened {
                key: proxy_key(),
                result: Ok((opened, SCALE)),
            },
            0,
        );
        world.pump_dst(accepted, 0);
        world.complete_parks(0);
        world.start_captures(0);
        assert_eq!(world.answers, vec![(0, world.actual)]);
        // The app's minimum only bites on later resizes: it opened at its own size.
        world.min = min;
        world
    }

    fn at(&mut self, now: u64) {
        assert!(now >= self.now, "time went backwards: {now} < {}", self.now);
        self.now = now;
    }

    /// What the destination produced: route messages to the source, apply host commands.
    fn pump_dst(&mut self, out: Vec<Output>, now: u64) {
        for output in out {
            match output {
                Output::SendControl {
                    msg: ControlMessage::Projection(msg),
                    ..
                } => {
                    if let Message::Resize {
                        request,
                        size,
                        scale,
                        ..
                    } = &msg
                    {
                        self.requests.push((*request, *size, *scale));
                    }
                    if self.link_up {
                        let mut out = Vec::new();
                        self.src.handle(&control(DST, msg), ms(now), &mut out);
                        self.pump_src(out, now);
                    }
                }
                // The host resizes the window (the window system may adjust the size), and
                // reports it, later. A command for the size it already has changes nothing.
                Output::ProxyGeometry { size, .. } if size != self.native => {
                    self.native = self.keeps.unwrap_or_else(|| {
                        px(size.width + self.adjust.0, size.height + self.adjust.1)
                    });
                    self.commands.push(size);
                    self.callbacks_due.push_back(now);
                }
                Output::ProxyFullscreen { fullscreen, .. }
                    if fullscreen != self.native_fullscreen =>
                {
                    self.set_native_state(fullscreen);
                    self.callbacks_due.push_back(now);
                }
                _ => {}
            }
        }
    }

    /// What the source produced: route messages to the destination, queue platform work.
    fn pump_src(&mut self, out: Vec<Output>, now: u64) {
        for output in out {
            match output {
                Output::SendControl {
                    msg: ControlMessage::Projection(msg),
                    ..
                } => {
                    if self.link_up {
                        if let Message::Geometry {
                            size,
                            answers,
                            fullscreen,
                            ..
                        } = &msg
                        {
                            self.answers.push((*answers, *size));
                            self.last_answer_state = fullscreen.map(|state| (*answers, state));
                        }
                        let out = self.dst.at(control(SRC, msg), now);
                        self.pump_dst(out, now);
                    }
                }
                Output::Park { size, .. } => {
                    self.parks.push_back((size, false, now));
                }
                Output::ResizeParked {
                    size, fullscreen, ..
                } => {
                    self.parks.push_back((size, fullscreen, now));
                }
                Output::StartCapture { projection, .. } => self.captures.push((projection, now)),
                _ => {}
            }
        }
    }

    /// The platform finishes every parking operation.
    fn complete_parks(&mut self, now: u64) {
        self.at(now);
        while self.complete_one_park(now) {}
    }

    /// The platform finishes the oldest parking operation: the window ends as large as asked,
    /// and at least as large as the app allows (or where the app resized itself to).
    fn complete_one_park(&mut self, now: u64) -> bool {
        self.at(now);
        let Some((asked, fullscreen, _)) = self.parks.pop_front() else {
            return false;
        };
        self.actual_fullscreen = fullscreen;
        self.actual = self.self_resize.take().unwrap_or_else(|| {
            px(
                asked.width.max(self.min.width),
                asked.height.max(self.min.height),
            )
        });
        let mut out = Vec::new();
        self.src.handle(
            &Input::Parked {
                window: WINDOW,
                result: Ok(Parked {
                    fullscreen,
                    ..parked(self.actual)
                }),
            },
            ms(now),
            &mut out,
        );
        self.pump_src(out, now);
        true
    }

    fn start_captures(&mut self, now: u64) {
        self.at(now);
        for (projection, _) in std::mem::take(&mut self.captures) {
            let mut out = Vec::new();
            self.src.handle(
                &Input::CaptureStarted {
                    projection,
                    result: Ok(StreamId(1)),
                },
                ms(now),
                &mut out,
            );
            self.pump_src(out, now);
        }
    }

    /// The user drags the proxy to `size`: the host's window is that size, and reports it.
    fn user_drag(&mut self, size: PixelSize, now: u64) {
        self.at(now);
        self.native = size;
        if !self.native_fullscreen {
            self.windowed = size;
        }
        self.report(now);
    }

    /// The host reports its window's native size as it is now. (Its contract, R2: a callback
    /// that was queued for an older change reports the current size, not the old one.)
    fn host_callback(&mut self, now: u64) {
        self.at(now);
        self.report(now);
    }

    fn report(&mut self, now: u64) {
        if self.reported_fullscreen != self.native_fullscreen {
            self.reported_fullscreen = self.native_fullscreen;
            let out = self.dst.at(
                Input::Proxy {
                    key: proxy_key(),
                    event: ProxyEvent::Fullscreen(self.native_fullscreen),
                },
                now,
            );
            self.pump_dst(out, now);
        }
        let out = self.dst.resized(self.native, SCALE, now);
        self.pump_dst(out, now);
    }

    fn set_native_state(&mut self, fullscreen: bool) {
        if fullscreen {
            self.windowed = self.native;
            self.native = px(1920, 1080);
        } else {
            self.native = self.windowed;
        }
        self.native_fullscreen = fullscreen;
    }

    fn user_toggle(&mut self, fullscreen: bool, now: u64) {
        self.at(now);
        if self.native_fullscreen != fullscreen {
            self.set_native_state(fullscreen);
            self.report(now);
        }
    }

    fn app_toggle(&mut self, fullscreen: bool, now: u64) {
        self.at(now);
        if !self.link_up
            || !self.parks.is_empty()
            || self.actual_fullscreen == fullscreen
            || self
                .last_self_resize
                .is_some_and(|last| now.saturating_sub(last) < 2_000)
        {
            return;
        }
        self.actual_fullscreen = fullscreen;
        let mut changed = window();
        changed.state = if fullscreen {
            WindowState::Fullscreen
        } else {
            WindowState::Normal
        };
        let mut out = Vec::new();
        self.src.handle(
            &Input::Windows(WindowEvent::Changed(changed)),
            ms(now),
            &mut out,
        );
        self.pump_src(out, now);
        self.last_self_resize = Some(now);
    }

    /// The host reports, once, for every resize it was commanded more than `after` ms ago.
    fn host_overdue(&mut self, now: u64, after: u64) {
        while self
            .callbacks_due
            .front()
            .is_some_and(|issued| now.saturating_sub(*issued) >= after)
        {
            self.callbacks_due.pop_front();
            self.host_callback(now);
        }
    }

    /// The platform is not unboundedly slow: finish parking and capture work older than `after`.
    fn platform_overdue(&mut self, now: u64, after: u64) {
        while self
            .parks
            .front()
            .is_some_and(|(_, _, issued)| now.saturating_sub(*issued) >= after)
        {
            self.complete_one_park(now);
        }
        if self
            .captures
            .iter()
            .any(|(_, issued)| now.saturating_sub(*issued) >= after)
        {
            self.start_captures(now);
        }
    }

    /// The app resizes its own window to `actual` (a bar appeared, a layout changed). Only if
    /// the source would take it up now (live, nothing running, 2 s since the last time).
    fn app_resizes_itself(&mut self, actual: PixelSize, now: u64) -> bool {
        self.at(now);
        if !self.link_up
            || !self.parks.is_empty()
            || self
                .last_self_resize
                .is_some_and(|last| now.saturating_sub(last) < 2_000)
        {
            return false;
        }
        self.frames += 1;
        let mut changed = window();
        changed.state = if self.actual_fullscreen {
            WindowState::Fullscreen
        } else {
            WindowState::Normal
        };
        changed.frame = RectLogical::new(
            PointLogical::new(0.0, f64::from(self.frames) * 3.0),
            SizeLogical::new(320.25, 240.25),
        );
        let mut out = Vec::new();
        self.src.handle(
            &Input::Windows(WindowEvent::Changed(changed)),
            ms(now),
            &mut out,
        );
        self.pump_src(out, now);
        if self.parks.is_empty() {
            return false;
        }
        self.self_resize = Some(actual);
        self.last_self_resize = Some(now);
        true
    }

    fn drop_link(&mut self, now: u64) {
        self.at(now);
        self.link_up = false;
        let mut out = Vec::new();
        self.src.handle(
            &Input::Link(LinkEvent::Closed {
                peer: DST,
                error: LinkError::Closed,
            }),
            ms(now),
            &mut out,
        );
        self.pump_src(out, now);
        self.dst.drop_link(now);
    }

    fn resume_link(&mut self, now: u64) {
        self.at(now);
        self.link_up = true;
        let out = self.dst.at(Input::PeerUp { peer: SRC }, now);
        self.pump_dst(out, now);
        let mut out = Vec::new();
        self.src
            .handle(&Input::PeerUp { peer: DST }, ms(now), &mut out);
        self.pump_src(out, now);
    }

    /// Tick both nodes at every deadline up to and including `to`.
    fn run_to(&mut self, to: u64) {
        self.at(self.now);
        assert!(to >= self.now, "time went backwards: {to} < {}", self.now);
        let mut finished = false;
        for _ in 0..10_000 {
            let deadline = [self.src.next_deadline(), self.dst.e2.next_deadline()]
                .into_iter()
                .flatten()
                .min();
            let Some(deadline) = deadline else {
                finished = true;
                break;
            };
            let at = (deadline.as_nanos() / 1_000_000).max(self.now);
            if at > to {
                finished = true;
                break;
            }
            self.now = at;
            let out = self.dst.tick(at);
            self.pump_dst(out, at);
            let mut out = Vec::new();
            self.src.handle(&Input::Tick, ms(at), &mut out);
            self.pump_src(out, at);
        }
        assert!(finished, "the deadline never advanced");
        self.now = to;
    }

    /// Let everything finish: the platform's work, the host's callbacks (promptly, from now
    /// on, each within 100 ms of its command), and the quiet periods, until nothing is
    /// outstanding and nothing has moved for 2 s.
    fn drain(&mut self) {
        assert!(self.link_up, "drain with the link down");
        let mut quiet = 0;
        for _ in 0..600 {
            let now = self.now;
            self.complete_parks(now);
            self.start_captures(now);
            self.host_overdue(now, 0);
            let idle =
                self.parks.is_empty() && self.captures.is_empty() && self.callbacks_due.is_empty();
            let before = (self.requests.len(), self.commands.len(), self.answers.len());
            self.run_to(now + 100);
            let after = (self.requests.len(), self.commands.len(), self.answers.len());
            if idle && before == after && self.parks.is_empty() && self.captures.is_empty() {
                quiet += 1;
                if quiet == 20 {
                    return;
                }
            } else {
                quiet = 0;
            }
        }
        panic!("the exchange never settled");
    }

    /// Once all work has finished, sizes agree or the host stays at the newest request's size
    /// while the source answers with its actual size. The newest request is answered and the
    /// exchange stays quiet for five seconds in either case.
    fn assert_converged(&mut self) {
        self.drain();
        let newest = self.requests.last().map_or(0, |(request, _, _)| *request);
        let last = *self.answers.last().expect("the source has answered");
        let settled_refusal = self.requests.last().is_some_and(|(request, size, _)| {
            self.native == *size
                && last.0 == *request
                && last.1 == self.actual
                && last.1 != self.native
        });
        assert!(
            self.native == self.actual || settled_refusal,
            "native window vs source window: {:?} vs {:?}",
            self.native,
            self.actual
        );
        assert_eq!(last.1, self.actual, "last answer vs source window");
        assert_eq!(last.0, newest, "last answer vs newest request");
        assert_eq!(
            self.native_fullscreen, self.actual_fullscreen,
            "proxy vs source state"
        );
        assert_eq!(
            self.last_answer_state,
            Some((newest, self.actual_fullscreen)),
            "last correlated state"
        );
        let counts = (self.requests.len(), self.commands.len(), self.answers.len());
        self.run_to(self.now + 5_000);
        assert!(self.parks.is_empty() && self.captures.is_empty());
        assert_eq!(
            counts,
            (self.requests.len(), self.commands.len(), self.answers.len()),
            "traffic after convergence"
        );
    }

    /// A refusing host and a constrained app may stay apart, but their difference is settled:
    /// the latest request is answered, all work is done, and repeated callbacks stay quiet.
    fn assert_keeping_converged(&mut self, size: PixelSize, actual: PixelSize) {
        self.drain();
        assert_eq!(self.native, size);
        assert_eq!(self.actual, actual);
        let newest = self.requests.last().map_or(0, |(request, _, _)| *request);
        assert_eq!(self.answers.last(), Some(&(newest, actual)));
        assert!(self.parks.is_empty() && self.captures.is_empty());
        assert!(self.callbacks_due.is_empty());
        let counts = (self.requests.len(), self.commands.len(), self.answers.len());
        for step in 1..=50 {
            self.run_to(self.now + 100);
            self.host_callback(self.now);
            assert!(
                self.parks.is_empty() && self.captures.is_empty(),
                "step {step}"
            );
            assert_eq!(
                counts,
                (self.requests.len(), self.commands.len(), self.answers.len()),
                "traffic after refusal at step {step}"
            );
        }
    }
}

#[test]
fn app_minimum_size_snaps_the_proxy_once() {
    // The user drags to 300x200; the app can't go below 400x300.
    let mut world = World::new(px(400, 300));
    world.user_drag(px(300, 200), 0);
    assert_eq!(world.requests, vec![(1, px(300, 200), SCALE)]);
    world.complete_parks(30);
    assert_eq!(world.answers, vec![(0, open_size()), (1, px(400, 300))]);
    // Nothing yet: the user counts as resizing until 250 ms after the last event.
    assert!(world.commands.is_empty());
    world.run_to(249);
    assert!(world.commands.is_empty());
    world.run_to(250);
    assert_eq!(world.commands, vec![px(400, 300)]);
    // The host's own resize completing is not a request, and nothing else follows.
    world.host_callback(260);
    assert_eq!(world.requests.len(), 1);
    world.assert_converged();
    assert_eq!(world.commands, vec![px(400, 300)]);
    assert_eq!(world.actual, px(400, 300));
}

#[test]
fn a_callback_that_does_not_match_is_user_intent_and_the_exchange_converges() {
    let mut world = World::new(px(400, 300));
    // The window system adds a pixel to the size it is asked for.
    world.adjust = (1, 0);
    world.user_drag(px(300, 200), 0);
    world.complete_parks(30);
    world.run_to(250);
    assert_eq!(world.commands, vec![px(400, 300)]);
    assert_eq!(world.native, px(401, 300));
    // Its callback reports 401x300, not what was asked: user intent.
    world.host_callback(260);
    assert_eq!(
        world.requests,
        vec![(1, px(300, 200), SCALE), (2, px(401, 300), SCALE)]
    );
    world.assert_converged();
    assert_eq!(world.requests.len(), 2);
    assert_eq!(world.commands, vec![px(400, 300)]);
    assert_eq!(world.actual, px(401, 300));
}

#[test]
fn constrained_then_back_to_the_requested_size_is_a_new_request() {
    let a = px(300, 200);
    let b = px(400, 300);
    let mut world = World::new(b);
    world.user_drag(a, 0);
    world.complete_parks(20);
    world.run_to(250);
    assert_eq!(world.commands, vec![b]);
    world.host_callback(260);
    assert_eq!(world.requests.len(), 1);
    // A second later the user drags to exactly A again. The source is at B, so that is a
    // genuine request for A (not a repeat of the one already sent).
    world.user_drag(a, 1_260);
    assert_eq!(world.requests, vec![(1, a, SCALE), (2, a, SCALE)]);
    // The source decides again (it can't), and its answer is applied once the user is quiet.
    world.assert_converged();
    assert_eq!(world.commands, vec![b, b]);
    assert_eq!(world.requests.len(), 2);
    assert_eq!(world.native, b);
}

#[test]
fn late_programmatic_callback_beyond_the_ttl_is_one_request_and_no_oscillation() {
    // The app minimum is 600x450, so the proxy is told to snap to it.
    let (asked, snapped) = (px(500, 400), px(600, 450));
    let mut world = World::new(snapped);
    world.user_drag(asked, 0);
    world.complete_parks(10);
    world.run_to(250);
    assert_eq!(world.commands, vec![snapped]);
    // The host's callback comes far too late to count as the host's own (COMMAND_TTL is 1 s).
    world.host_callback(1_800);
    assert_eq!(world.requests, vec![(1, asked, SCALE), (2, snapped, SCALE)]);
    // The source is already there: it answers at once, without any platform work.
    assert!(world.parks.is_empty());
    assert_eq!(world.answers.last(), Some(&(2, snapped)));
    world.assert_converged();
    assert_eq!(world.requests.len(), 2);
    assert_eq!(world.commands, vec![snapped]);
}

#[test]
fn late_callbacks_reported_in_a_burst_after_two_commands_converge() {
    // The source's app resizes itself to A, and 2 s later to B (commands A then B). The host's
    // callbacks for both are late and come together: each reports the native size, which is B.
    let (a, b) = (px(700, 500), px(720, 520));
    let mut world = World::new(px(1, 1));
    assert!(world.app_resizes_itself(a, 0));
    world.complete_parks(0);
    assert!(world.app_resizes_itself(b, 2_000));
    world.complete_parks(2_000);
    assert_eq!(world.commands, vec![a, b]);
    world.host_overdue(2_050, 0);
    // B's command is what the host reports (and confirms); the other report changes nothing.
    assert!(world.requests.is_empty());
    world.assert_converged();
    assert_eq!(world.native, b);
}

#[test]
fn a_drag_onto_a_superseded_size_after_the_newest_confirms_converges() {
    // The swallowed-intent trace through the source model: commands A then B (2 s apart, the
    // source's pace), B's callback, then a genuine drag to A.
    let (a, b) = (px(700, 500), px(720, 520));
    let mut world = World::new(px(1, 1));
    assert!(world.app_resizes_itself(a, 0));
    world.complete_parks(0);
    assert!(world.app_resizes_itself(b, 2_000));
    world.complete_parks(2_000);
    world.host_overdue(2_050, 0);
    assert!(world.requests.is_empty());
    world.user_drag(a, 2_100);
    assert_eq!(world.requests, vec![(1, a, SCALE)]);
    world.assert_converged();
    assert_eq!(world.native, a);
}

#[test]
fn a_drag_back_after_the_ttl_with_a_delayed_callback_is_not_swallowed() {
    // A4, source-initiated: the window goes to A and the host's callback is delayed.
    let a = px(700, 500);
    let mut world = World::new(px(1, 1));
    assert!(world.app_resizes_itself(a, 0));
    world.complete_parks(0);
    assert_eq!(world.commands, vec![a]);
    // 1001 ms: past COMMAND_TTL, and still nothing from the host. The user drags back to the
    // size the engine last heard about. It is not "unchanged": a command is unconfirmed.
    world.user_drag(open_size(), 1_001);
    assert_eq!(world.requests, vec![(1, open_size(), SCALE)]);
    // The delayed callback samples the native size, which is back at the original.
    world.host_callback(1_002);
    world.assert_converged();
    assert_eq!(world.native, open_size());
    assert_eq!(world.actual, open_size());

    // A4, request-driven: the user asks for 300x200, the app snaps to 400x300, the proxy is
    // commanded to it at 250 ms, and the callback never comes. At 1500 ms the user drags back
    // to 300x200.
    let (a, b) = (px(300, 200), px(400, 300));
    let mut world = World::new(b);
    world.user_drag(a, 0);
    world.complete_parks(10);
    world.run_to(250);
    assert_eq!(world.commands, vec![b]);
    world.user_drag(a, 1_500);
    assert_eq!(world.requests, vec![(1, a, SCALE), (2, a, SCALE)]);
    world.assert_converged();
    assert_eq!(world.native, b);
}

#[test]
fn a_drag_back_onto_the_stale_current_while_a_host_resize_is_unconfirmed_is_user_intent() {
    // Deviation (a), M5: the user's size is `current` (its callback is the last thing the engine
    // heard), the app snaps to 400x300, and the command goes out at 251 ms. The user drags back
    // onto exactly the size the engine still believes in, before the host reports the command.
    // Ignoring that as "unchanged" would leave the window at 200x150 and the source at 400x300
    // with nothing in flight.
    let (a, b) = (px(200, 150), px(400, 300));
    let mut world = World::new(b);
    world.user_drag(a, 1);
    world.complete_parks(90);
    world.run_to(251);
    assert_eq!(world.commands, vec![b]);
    world.user_drag(a, 251);
    assert_eq!(world.requests, vec![(1, a, SCALE), (2, a, SCALE)]);
    world.assert_converged();
    assert_eq!(world.native, b);
    assert_eq!(world.actual, b);
}

#[test]
fn resume_after_a_discarded_queued_request_re_requests_and_converges_on_a_constrained_size() {
    // A1. Request 1 (A) is running on the source when request 2 (B, below the app's minimum)
    // reaches it and is queued. The link drops, the source discards request 2, and request 1
    // finishes while suspended. On resume the source re-parks to B and hits the minimum C.
    let (a, b, c) = (px(700, 500), px(300, 200), px(400, 300));
    let mut world = World::new(c);
    world.user_drag(a, 0);
    world.user_drag(b, 50);
    assert_eq!(world.requests, vec![(1, a, SCALE), (2, b, SCALE)]);
    world.drop_link(60);
    world.complete_parks(70);
    assert_eq!(world.actual, a);
    world.resume_link(80);
    // The resume asks again, for the size the proxy has, with the next number.
    assert_eq!(world.requests.last(), Some(&(3, b, SCALE)));
    world.complete_parks(100);
    world.start_captures(100);
    world.assert_converged();
    // The source's restart geometry still said "answers 1" (request 2 was discarded), which the
    // destination rightly ignored; the answer to request 3 is what brought the proxy to C.
    assert!(world.answers.contains(&(1, c)));
    assert_eq!(world.answers.last(), Some(&(3, c)));
    assert_eq!(world.native, c);
    assert_eq!(world.actual, c);
    assert_eq!(world.requests.len(), 3);
}

#[test]
fn resume_with_nothing_to_change_is_answered_at_once() {
    // The resumed size is the window's: the re-request is satisfied without platform work.
    let mut world = World::new(px(1, 1));
    world.drop_link(10);
    world.resume_link(20);
    assert_eq!(world.requests, vec![(1, open_size(), SCALE)]);
    world.assert_converged();
}

#[test]
fn an_old_source_never_resizes_the_proxy_after_a_resume_either() {
    // The re-request is numbered; a source that predates numbers never answers it, and the
    // proxy stays as the user left it (degraded but stable).
    let mut dst = Dst::new();
    dst.drop_link(10);
    let out = dst.resume(20);
    assert_eq!(
        messages(&out),
        vec![
            Message::Accepted {
                projection: ID,
                size: open_size(),
                scale: SCALE
            },
            resize(1, open_size(), SCALE)
        ]
    );
    assert!(traffic(&dst.geometry(px(900, 700), 0, 30)).is_empty());
    assert!(dst.run(30, 5_000).is_empty());
}

// ---- The source alone ----

/// A source with its window live, parked at `open_size()` and `SCALE`.
struct Src {
    e2: E2,
}

impl Src {
    fn live() -> Self {
        let mut src = Self { e2: node(SRC, DST) };
        src.at(Input::Windows(WindowEvent::Added(window())), 0);
        src.at(Input::Windows(WindowEvent::Focused(Some(WINDOW))), 0);
        src.at(
            Input::Command(Command::Project {
                window: WINDOW,
                to: DST,
                place: None,
            }),
            0,
        );
        src.at(
            control(
                DST,
                Message::Accepted {
                    projection: ID,
                    size: open_size(),
                    scale: SCALE,
                },
            ),
            0,
        );
        src.at(
            Input::Parked {
                window: WINDOW,
                result: Ok(parked(open_size())),
            },
            0,
        );
        src.at(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(StreamId(1)),
            },
            0,
        );
        src
    }
    /// A source that has offered the window and is waiting for `Accepted` (stage 0); the
    /// caller walks it through parking and capture.
    fn offered() -> Self {
        let mut src = Self { e2: node(SRC, DST) };
        src.at(Input::Windows(WindowEvent::Added(window())), 0);
        src.at(Input::Windows(WindowEvent::Focused(Some(WINDOW))), 0);
        src.at(
            Input::Command(Command::Project {
                window: WINDOW,
                to: DST,
                place: None,
            }),
            0,
        );
        src
    }
    fn at(&mut self, input: Input, now: u64) -> Vec<Output> {
        let mut out = Vec::new();
        self.e2.handle(&input, ms(now), &mut out);
        out
    }
    fn request(&mut self, request: u32, size: PixelSize, scale: f64, now: u64) -> Vec<Output> {
        self.at(control(DST, resize(request, size, scale)), now)
    }
    fn parked(&mut self, size: PixelSize, now: u64) -> Vec<Output> {
        self.at(
            Input::Parked {
                window: WINDOW,
                result: Ok(parked(size)),
            },
            now,
        )
    }
}

fn resize_parked(size: PixelSize, scale: f64) -> Output {
    Output::ResizeParked {
        fullscreen: false,
        window: WINDOW,
        size,
        scale,
    }
}

/// The messages `out` sends to the destination.
fn answers(out: &[Output]) -> Vec<(u32, PixelSize)> {
    messages(out)
        .into_iter()
        .filter_map(|m| match m {
            Message::Geometry {
                size, answers: n, ..
            } => Some((n, size)),
            _ => None,
        })
        .collect()
}

#[test]
fn source_parked_completion_answers_the_request_in_flight_not_the_queued_one() {
    let mut src = Src::live();
    let (a, b, c) = (px(800, 600), px(900, 700), px(1000, 800));
    assert_eq!(src.request(1, a, SCALE, 10), vec![resize_parked(a, SCALE)]);
    // Two more while it runs: the newest wins, and the superseded one is never answered.
    assert!(src.request(2, b, SCALE, 11).is_empty());
    assert!(src.request(3, c, SCALE, 12).is_empty());
    let out = src.parked(a, 20);
    assert_eq!(answers(&out), vec![(1, a)]);
    assert_eq!(out.last(), Some(&resize_parked(c, SCALE)));
    let out = src.parked(c, 30);
    assert_eq!(answers(&out), vec![(3, c)]);
}

#[test]
fn source_answers_a_satisfied_request_at_once() {
    let mut src = Src::live();
    // Already this size and scale: no platform work, just the answer.
    let out = src.request(1, open_size(), SCALE, 10);
    assert_eq!(out, vec![sent(DST, actual_geometry(open_size(), 1))]);
    // A different scale is not satisfied.
    assert_eq!(
        src.request(2, open_size(), 1.0, 11),
        vec![resize_parked(open_size(), 1.0)]
    );
}

#[test]
fn source_compares_the_actual_size_not_what_was_asked_for() {
    let mut src = Src::live();
    let (a, b) = (px(300, 200), px(400, 300));
    assert_eq!(src.request(1, a, SCALE, 10), vec![resize_parked(a, SCALE)]);
    // The app's minimum: the window ends at B.
    assert_eq!(answers(&src.parked(b, 20)), vec![(1, b)]);
    // A again: A was asked for before, but the window is at B, so it is not satisfied.
    assert_eq!(src.request(2, a, SCALE, 30), vec![resize_parked(a, SCALE)]);
    assert_eq!(answers(&src.parked(b, 40)), vec![(2, b)]);
    // B itself is satisfied, by the actual size.
    assert_eq!(
        src.request(3, b, SCALE, 50),
        vec![sent(DST, actual_geometry(b, 3))]
    );
}

#[test]
fn source_coalesced_request_is_not_answered_on_its_own() {
    let mut src = Src::live();
    let (a, b, c) = (px(800, 600), px(900, 700), px(1000, 800));
    src.request(1, a, SCALE, 10);
    // 2 is superseded in the queue by 3: only 3 is ever answered.
    assert!(src.request(2, b, SCALE, 11).is_empty());
    assert!(src.request(3, c, SCALE, 12).is_empty());
    let mut every = answers(&src.parked(a, 20));
    every.extend(answers(&src.parked(c, 30)));
    assert_eq!(every, vec![(1, a), (3, c)]);
}

#[test]
fn source_queued_request_that_is_already_satisfied_is_answered_after_the_one_in_flight() {
    let mut src = Src::live();
    let (a, b) = (px(800, 600), px(900, 700));
    src.request(1, a, SCALE, 10);
    assert!(src.request(2, b, SCALE, 11).is_empty());
    // The newest queued request asks for the size the window is about to have.
    assert!(src.request(3, a, SCALE, 12).is_empty());
    let out = src.parked(a, 20);
    assert_eq!(
        messages(&out),
        vec![actual_geometry(a, 1), actual_geometry(a, 3)]
    );
    // No platform work for it, and it can't be answered a second time.
    assert!(!out.iter().any(|o| matches!(o, Output::ResizeParked { .. })));
}

#[test]
fn source_ignores_stale_and_duplicate_requests() {
    let mut src = Src::live();
    let (a, b) = (px(800, 600), px(900, 700));
    assert_eq!(src.request(5, a, SCALE, 10), vec![resize_parked(a, SCALE)]);
    assert_eq!(answers(&src.parked(a, 20)), vec![(5, a)]);
    // A duplicate and an older one: ignored.
    assert!(src.request(5, b, SCALE, 30).is_empty());
    assert!(src.request(3, b, SCALE, 31).is_empty());
    // Newer: processed.
    assert_eq!(src.request(6, b, SCALE, 32), vec![resize_parked(b, SCALE)]);
    // While one runs, an older one doesn't displace a newer queued one.
    assert!(src.request(8, a, SCALE, 33).is_empty());
    assert!(src.request(7, open_size(), SCALE, 34).is_empty());
    let out = src.parked(b, 40);
    assert_eq!(answers(&out), vec![(6, b)]);
    assert_eq!(out.last(), Some(&resize_parked(a, SCALE)));
    assert_eq!(answers(&src.parked(a, 50)), vec![(8, a)]);
}

#[test]
fn source_request_zero_is_always_newest_and_answered_with_zero() {
    let mut src = Src::live();
    let (a, b) = (px(800, 600), px(900, 700));
    // A destination that predates request numbers sends 0 every time.
    assert_eq!(src.request(0, a, SCALE, 10), vec![resize_parked(a, SCALE)]);
    assert_eq!(answers(&src.parked(a, 20)), vec![(0, a)]);
    assert_eq!(src.request(0, b, SCALE, 30), vec![resize_parked(b, SCALE)]);
    assert_eq!(answers(&src.parked(b, 40)), vec![(0, b)]);
    // Satisfied ones are answered with 0 too, and it is not stale however often it repeats.
    assert_eq!(
        src.request(0, b, SCALE, 50),
        vec![sent(DST, actual_geometry(b, 0))]
    );
    // Even after numbered requests, 0 isn't stale, and its answer is 0.
    assert_eq!(src.request(4, a, SCALE, 60), vec![resize_parked(a, SCALE)]);
    assert_eq!(answers(&src.parked(a, 70)), vec![(4, a)]);
    assert_eq!(
        src.request(0, a, SCALE, 75),
        vec![sent(DST, actual_geometry(a, 0))]
    );
    assert_eq!(src.request(0, b, SCALE, 80), vec![resize_parked(b, SCALE)]);
    assert_eq!(answers(&src.parked(b, 90)), vec![(0, b)]);
    assert_eq!(
        src.request(0, b, SCALE, 100),
        vec![sent(DST, actual_geometry(b, 0))]
    );
}

#[test]
fn source_geometry_changes_on_its_own_carry_the_current_answered() {
    let mut src = Src::live();
    let a = px(800, 600);
    src.request(1, a, SCALE, 10);
    assert_eq!(answers(&src.parked(a, 20)), vec![(1, a)]);
    // The window moves by itself (a bar appeared): a re-park, which answers nothing new.
    let mut moved = window();
    moved.frame = RectLogical::new(
        PointLogical::new(0.0, 26.0),
        SizeLogical::new(320.25, 214.25),
    );
    let out = src.at(Input::Windows(WindowEvent::Changed(moved)), 3_000);
    assert_eq!(out, vec![resize_parked(a, SCALE)]);
    assert_eq!(answers(&src.parked(a, 3_010)), vec![(1, a)]);
}

#[test]
fn source_answers_a_refused_size_only_after_older_work_and_without_platform_work() {
    // A2. A numbered Resize with a size outside the sane range is refused, but it is still a
    // request: it advances the watermark, replaces a queued one, and is answered with the actual
    // size once nothing older is in flight.
    let (a, b) = (px(800, 600), px(900, 700));
    let bad = px(0, 480);
    let huge = px(16_385, 480);
    for refused in [bad, huge] {
        let mut src = Src::live();
        assert_eq!(src.request(1, a, SCALE, 10), vec![resize_parked(a, SCALE)]);
        // Queued behind request 1: not answered before it completes.
        assert!(src.request(2, refused, SCALE, 11).is_empty());
        let out = src.parked(a, 20);
        assert_eq!(
            messages(&out),
            vec![actual_geometry(a, 1), actual_geometry(a, 2)]
        );
        assert!(!out.iter().any(|o| matches!(o, Output::ResizeParked { .. })));
        // The watermark moved: 2 again and older ones are stale; the next one is processed.
        assert!(src.request(2, b, SCALE, 30).is_empty());
        assert!(src.request(1, b, SCALE, 31).is_empty());
        assert_eq!(src.request(3, b, SCALE, 32), vec![resize_parked(b, SCALE)]);
    }
    // Idle and live: answered at once, with the actual size.
    let mut src = Src::live();
    assert_eq!(
        src.request(1, bad, SCALE, 10),
        vec![sent(DST, actual_geometry(open_size(), 1))]
    );
    // A refused request replaces a queued valid one (which then does no platform work), and a
    // valid one replaces a queued refused one (which is then never answered).
    let mut src = Src::live();
    src.request(1, a, SCALE, 10);
    assert!(src.request(2, b, SCALE, 11).is_empty());
    assert!(src.request(3, bad, SCALE, 12).is_empty());
    let out = src.parked(a, 20);
    assert_eq!(
        messages(&out),
        vec![actual_geometry(a, 1), actual_geometry(a, 3)]
    );
    assert!(!out.iter().any(|o| matches!(o, Output::ResizeParked { .. })));
    let mut src = Src::live();
    src.request(1, a, SCALE, 10);
    assert!(src.request(2, bad, SCALE, 11).is_empty());
    assert!(src.request(3, b, SCALE, 12).is_empty());
    let out = src.parked(a, 20);
    assert_eq!(messages(&out), vec![actual_geometry(a, 1)]);
    assert_eq!(out.last(), Some(&resize_parked(b, SCALE)));
    // After a refused request 10, request 9 is stale (S3).
    let mut src = Src::live();
    assert_eq!(
        src.request(10, bad, SCALE, 10),
        vec![sent(DST, actual_geometry(open_size(), 10))]
    );
    assert!(src.request(9, a, SCALE, 11).is_empty());
    // A destination that predates numbers (0) gets 0 back.
    let mut src = Src::live();
    assert_eq!(
        src.request(0, bad, SCALE, 10),
        vec![sent(DST, actual_geometry(open_size(), 0))]
    );
}

#[test]
fn source_answers_a_refused_size_queued_before_it_is_live() {
    let bad = px(0, 0);
    for stage in 0..3 {
        // Offered, Parking, Capturing: queued, and answered the moment the window goes live.
        let mut src = Src::offered();
        // Walk it up to the stage (0: offered, 1: parking, 2: capturing), queue the request
        // there, then finish the remaining steps.
        let accept = |src: &mut Src, now| {
            src.at(
                control(
                    DST,
                    Message::Accepted {
                        projection: ID,
                        size: open_size(),
                        scale: SCALE,
                    },
                ),
                now,
            )
        };
        if stage >= 1 {
            accept(&mut src, 1);
        }
        if stage >= 2 {
            src.parked(open_size(), 2);
        }
        assert!(src.request(1, bad, SCALE, 5).is_empty());
        if stage < 1 {
            accept(&mut src, 6);
        }
        if stage < 2 {
            src.parked(open_size(), 7);
        }
        let out = src.at(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(StreamId(1)),
            },
            8,
        );
        assert_eq!(messages(&out), vec![actual_geometry(open_size(), 1)]);
        assert!(!out.iter().any(|o| matches!(o, Output::ResizeParked { .. })));
    }
}

// ---- The invariant, over arbitrary interleavings ----

#[test]
fn a_host_that_keeps_its_tile_size_converges() {
    let (tile, minimum) = (px(530, 1886), px(800, 1886));
    let mut world = World::new(minimum);
    world.keeps = Some(tile);
    world.user_drag(tile, 0);
    assert_eq!(world.requests, [(1, tile, SCALE)]);
    world.complete_parks(20);
    world.run_to(250);
    assert_eq!(world.commands, [minimum]);
    assert_eq!(world.native, tile);
    world.host_overdue(251, 0);
    assert_eq!(world.requests, [(1, tile, SCALE), (2, tile, SCALE)]);
    world.complete_parks(260);
    world.run_to(501);
    assert_eq!(world.commands, [minimum, minimum]);
    world.host_overdue(502, 0);
    world.assert_keeping_converged(tile, minimum);
    assert_eq!(world.requests.len(), 2);
    assert_eq!(world.commands.len(), 2);
}

#[test]
fn a_tile_smaller_than_the_window_at_open_converges() {
    let (tile, minimum) = (px(530, 1886), px(800, 1886));
    let mut world = World::open_at(minimum, tile, Some(tile));
    assert_eq!(world.native, tile);
    assert_eq!(world.actual, minimum);
    assert_eq!(world.commands, [minimum]);
    world.assert_keeping_converged(tile, minimum);
    assert_eq!(
        world.requests,
        [(1, tile, SCALE)],
        "first Resize asks for the tile, not the answer"
    );
    assert_eq!(world.commands.len(), 2);
}

fn keeping_world() -> World {
    let (tile, minimum) = (px(530, 1886), px(800, 1886));
    let mut world = World::new(minimum);
    world.keeps = Some(tile);
    world.user_drag(tile, 0);
    world.assert_keeping_converged(tile, minimum);
    world
}

#[test]
fn after_a_refusal_a_new_user_size_resumes_normal_exchange() {
    let mut world = keeping_world();
    let before = world.requests.len();
    let floated = px(900, 1950);
    world.keeps = None;
    world.user_drag(floated, world.now + 1);
    assert_eq!(world.requests.len(), before + 1);
    assert_eq!(world.requests.last(), Some(&(3, floated, SCALE)));
    world.assert_converged();
    assert_eq!(world.native, floated);
}

#[test]
fn after_a_refusal_a_source_resize_is_applied() {
    for obeys in [false, true] {
        let mut world = keeping_world();
        if obeys {
            world.keeps = None;
        }
        let changed = px(900, 2000);
        let before = (world.requests.len(), world.commands.len());
        assert!(world.app_resizes_itself(changed, world.now + 1));
        world.complete_parks(world.now + 1);
        assert_eq!(
            &world.commands[before.1..],
            &[changed],
            "source change is commanded immediately once"
        );
        if obeys {
            world.assert_converged();
            assert_eq!(world.native, changed);
            assert_eq!(world.requests.len(), before.0);
        } else {
            world.assert_keeping_converged(px(530, 1886), px(800, 1886));
            assert!(world.requests.len() - before.0 <= 2);
        }
        assert_eq!(
            world.commands[before.1..]
                .iter()
                .filter(|size| **size == changed)
                .count(),
            1
        );
    }
}

fn settled_destination_refusal() -> Dst {
    let (tile, minimum) = (px(530, 1886), px(800, 1886));
    let mut dst = Dst::new();
    assert_eq!(
        messages(&dst.resized(tile, SCALE, 0)),
        [resize(1, tile, SCALE)]
    );
    assert!(dst.geometry(minimum, 1, 10).is_empty());
    assert_eq!(proxy_geometries(&dst.tick(250)), [minimum]);
    assert_eq!(
        messages(&dst.resized(tile, SCALE, 251)),
        [resize(2, tile, SCALE)]
    );
    assert!(dst.geometry(minimum, 2, 260).is_empty());
    assert_eq!(proxy_geometries(&dst.tick(501)), [minimum]);
    assert!(dst.resized(tile, SCALE, 502).is_empty());
    dst
}

#[test]
fn after_a_refusal_a_scale_change_starts_a_fresh_exchange() {
    let (tile, minimum) = (px(530, 1886), px(800, 1886));
    for obeys in [false, true] {
        let mut dst = settled_destination_refusal();
        assert_eq!(
            messages(&dst.resized(tile, 1.0, 600)),
            [resize(3, tile, 1.0)]
        );
        assert!(dst.geometry(minimum, 3, 610).is_empty());
        assert_eq!(proxy_geometries(&dst.tick(850)), [minimum]);
        if obeys {
            assert!(dst.resized(minimum, 1.0, 851).is_empty());
        } else {
            assert_eq!(
                messages(&dst.resized(tile, 1.0, 851)),
                [resize(4, tile, 1.0)]
            );
            assert!(dst.geometry(minimum, 4, 860).is_empty());
            assert_eq!(proxy_geometries(&dst.tick(1101)), [minimum]);
            assert!(dst.resized(tile, 1.0, 1102).is_empty());
        }
        assert!(dst.run(dst.last, dst.last + 5_000).is_empty());
    }
}

#[test]
fn after_a_refusal_suspend_and_resume_restore_normal_exchange() {
    let mut world = keeping_world();
    let before = (world.requests.len(), world.commands.len());
    let now = world.now + 1;
    world.drop_link(now);
    world.keeps = None;
    world.resume_link(now + 10);
    assert_eq!(world.requests.len(), before.0 + 1);
    assert_eq!(world.requests.last(), Some(&(3, px(530, 1886), SCALE)));
    world.assert_converged();
    assert_eq!(world.native, px(800, 1886));
    assert_eq!(world.commands.len(), before.1 + 1);
}

#[test]
fn repeated_and_stale_geometry_do_not_restart_a_settled_refusal() {
    let mut dst = settled_destination_refusal();
    assert!(traffic(&dst.geometry(px(900, 2000), 1, 600)).is_empty());
    assert!(traffic(&dst.geometry(px(800, 1886), 2, 610)).is_empty());
    assert!(dst.run(610, 5_610).is_empty());
    assert!(dst.resized(px(530, 1886), SCALE, 5_611).is_empty());
}

#[test]
fn repeated_drags_before_host_callbacks_settle_refusal_and_new_size_resumes_exchange() {
    let late = 4_000;
    let ops = [(0, 0, 1), (4, 0, 89), (0, 0, 161), (4, 0, 9), (0, 0, 241)];
    let refused = px(200, 150);
    let minimum = px(400, 300);
    let mut world = World::new(minimum);
    let mut now = 0;
    for (kind, _arg, dt) in ops {
        now += dt;
        world.run_to(now);
        world.host_overdue(now, late);
        world.platform_overdue(now, 3_000);
        match kind {
            0 => world.user_drag(refused, now),
            4 => {
                world.complete_one_park(now);
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(world.keeps, None);
    assert_eq!(world.native, refused);
    assert_eq!(world.actual, minimum);
    assert_eq!(
        world.requests,
        vec![(1, refused, SCALE), (2, refused, SCALE)]
    );
    assert_eq!(world.answers.last(), Some(&(2, minimum)));
    assert_eq!(world.commands, vec![minimum, minimum]);
    world.assert_converged();
    assert_eq!(world.native, refused);
    assert_eq!(world.actual, minimum);
    assert_eq!(world.requests.len(), 2);
    assert_eq!(world.commands.len(), 2);

    let next = px(777, 433);
    world.user_drag(next, world.now + 1);
    assert_eq!(world.requests.last(), Some(&(3, next, SCALE)));
    world.assert_converged();
    assert_eq!(world.native, next);
    assert_eq!(world.actual, next);
    assert_eq!(world.answers.last(), Some(&(3, next)));
}

#[test]
fn user_cancels_app_fullscreen_before_host_confirmation_and_converges() {
    let mut world = World::new(px(400, 300));
    world.app_toggle(true, 1);
    assert_eq!(world.parks.front(), Some(&(open_size(), true, 1)));
    world.complete_one_park(2);
    assert!(world.actual_fullscreen && world.native_fullscreen);
    assert!(!world.reported_fullscreen);
    world.user_toggle(false, 3);
    assert!(!world.native_fullscreen);
    eprintln!(
        "before drain: requests={:?}, answers={:?}, state={:?}, parks={:?}, native={}, source={}, deadlines={:?}/{:?}",
        world.requests,
        world.answers,
        world.last_answer_state,
        world.parks,
        world.native_fullscreen,
        world.actual_fullscreen,
        world.src.next_deadline(),
        world.dst.e2.next_deadline()
    );
    world.assert_converged();
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 500, failure_persistence: None, ..ProptestConfig::default() })]

    /// Whatever order the user's drags, the app's own resizes, the platform's completions, link
    /// drops and resumes, and the host's callbacks (prompt or arbitrarily late, repeated or
    /// strayed) come in, once everything outstanding has run its course the proxy's native
    /// window, the source's window and the last answer the source really sent are the same size,
    /// and nothing more is ever sent.
    #[test]
    fn resize_exchanges_converge_without_oscillation(
        ops in proptest::collection::vec((0u8..11, 0usize..8, 1u64..400), 1..40),
        late in prop_oneof![Just(500u64), Just(1_500), Just(4_000)],
    ) {
        let sizes = [
            px(200, 150),
            px(300, 200),
            px(400, 300),
            px(401, 300),
            px(500, 400),
            px(640, 480),
            px(777, 433),
            px(900, 700),
        ];
        let mut world = World::new(px(400, 300));
        let mut now = 0;
        for (kind, arg, dt) in ops {
            now += dt;
            world.run_to(now);
            // The host and the platform are slow, but not unboundedly so.
            world.host_overdue(now, late);
            world.platform_overdue(now, 3_000);
            match kind {
                0 | 1 => world.user_drag(sizes[arg], now),
                2 => {
                    world.callbacks_due.pop_front();
                    world.host_callback(now);
                }
                3 => world.host_callback(now),
                4 => {
                    world.complete_one_park(now);
                }
                5 => {
                    world.app_resizes_itself(sizes[arg], now);
                }
                6 if world.link_up => world.drop_link(now),
                7 if !world.link_up => world.resume_link(now),
                9 => world.user_toggle(arg % 2 == 0, now),
                10 => world.app_toggle(arg % 2 == 0, now),
                _ => world.start_captures(now),
            }
        }
        if !world.link_up {
            world.resume_link(now);
        }
        world.assert_converged();
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 500, failure_persistence: None, ..ProptestConfig::default() })]

    /// Fixed host/source sizes converge even with callbacks beyond the command TTL, repeated
    /// callbacks, and slow parking completions. The old obeying-host property remains untouched.
    #[test]
    fn keeping_host_resize_exchanges_converge_with_bounded_traffic(
        width in 100u32..800,
        height in 100u32..2000,
        extra in 1u32..1000,
        opened in any::<bool>(),
        delays in proptest::collection::vec((1u64..2500, any::<bool>(), any::<bool>()), 1..20),
    ) {
        let tile = px(width, height);
        let minimum = px(width + extra, height);
        let mut world = if opened {
            World::open_at(minimum, tile, Some(tile))
        } else {
            let mut world = World::new(minimum);
            world.keeps = Some(tile);
            world.user_drag(tile, 0);
            world
        };
        for (dt, callback, complete) in delays {
            let now = world.now + dt;
            world.run_to(now);
            world.platform_overdue(now, 3_000);
            if complete {
                world.complete_parks(now);
                world.start_captures(now);
            }
            if callback {
                world.host_overdue(now, 0);
                world.host_callback(now);
            }
        }
        world.assert_keeping_converged(tile, minimum);
        prop_assert!(world.requests.len() <= 2, "{:?}", world.requests);
        prop_assert!(world.commands.len() <= 2, "{:?}", world.commands);
    }
}
