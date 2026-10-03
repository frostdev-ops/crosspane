//! WP-2.45b: a source window's state change (Normal to Fullscreen or Hidden and back) is a
//! re-park trigger, a frame move while the window is not `Normal` is not, and windows that are
//! `Hidden` are not offered for browsing. A change that can't be followed at once (a park in
//! flight, the `REPARK_GAP`) is deferred, never dropped: it is looked at again when the park
//! finishes and when the gap ends, with no further window event needed. The source role alone,
//! driven by window events and platform results; all time is explicit and nothing sleeps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_engine::e2::E2;
use crosspane_engine::{Command, EngineConfig, Input, Output, ProjectionKey, ProxyEvent};
use crosspane_input::journal::MemoryJournal;
use crosspane_platform::{
    CaptureTarget, LockState, Parked, ParkingKind as PlatformParking, SessionEvent, SessionState,
    StreamEndReason, StreamId, WindowEvent, WindowInfo, WindowRole, WindowState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ControlMessage};
use crosspane_protocol::projection::{
    ParkingKind, ProjectionEndReason as Reason, ProjectionMessage as Message, WindowSummary,
};
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
const SCALE: f64 = 2.0;
const STREAM: StreamId = StreamId(1);

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
/// The whole twin display, in device pixels (what a fullscreen window fills).
fn whole() -> PixelSize {
    px(1710, 1406)
}
fn control(peer: NodeId, msg: Message) -> Input {
    Input::Link(LinkEvent::Control {
        peer,
        msg: ControlMessage::Projection(msg),
    })
}
fn sent(peer: NodeId, msg: Message) -> Output {
    Output::SendControl {
        peer,
        msg: ControlMessage::Projection(msg),
    }
}
fn geometry(size: PixelSize, answers: u32) -> Message {
    Message::Geometry {
        fullscreen: Some(false),
        projection: ID,
        size,
        parking: ParkingKind::Twin,
        answers,
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
/// The `Geometry` messages in `out`: (answers, size).
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
fn reparks(out: &[Output]) -> usize {
    out.iter()
        .filter(|o| matches!(o, Output::ResizeParked { .. }))
        .count()
}

/// The window's frame while it is `Normal`.
fn normal_frame() -> RectLogical {
    RectLogical::new(PointLogical::zero(), SizeLogical::new(320.25, 240.25))
}
/// The window's frame while it fills the twin display.
fn whole_frame() -> RectLogical {
    RectLogical::new(PointLogical::zero(), SizeLogical::new(855.0, 703.0))
}
fn window_in(state: WindowState, frame: RectLogical) -> WindowInfo {
    WindowInfo {
        id: WINDOW,
        title: "fixture".into(),
        app_id: "test".into(),
        pid: None,
        display: Some(DISPLAY),
        frame,
        state,
        role: WindowRole::Toplevel,
        parent: None,
    }
}
fn window() -> WindowInfo {
    window_in(WindowState::Normal, normal_frame())
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

fn node() -> E2 {
    let (mut e2, out) = E2::new(
        &EngineConfig::new(SRC),
        Box::new(MemoryJournal::default()),
        ms(0),
    )
    .unwrap();
    assert!(out.is_empty());
    let mut sink = Vec::new();
    e2.handle(&Input::Session(SessionEvent::State(OPEN)), ms(0), &mut sink);
    e2.handle(&Input::PeerUp { peer: DST }, ms(0), &mut sink);
    e2.handle(
        &Input::Grants(
            [(
                DST,
                [
                    Capability::WindowBrowse,
                    Capability::WindowShare,
                    Capability::WindowPresent,
                ]
                .into(),
            )]
            .into(),
        ),
        ms(0),
        &mut sink,
    );
    e2
}

/// A source with its window live, parked at `open_size()` and `SCALE` at time 0.
struct Src {
    e2: E2,
}

impl Src {
    fn live() -> Self {
        Self::live_in(window())
    }
    /// The window is `info` when it is projected, so the first park records that state.
    fn live_in(info: WindowInfo) -> Self {
        let fullscreen = info.state == WindowState::Fullscreen;
        let mut src = Self { e2: node() };
        src.at(Input::Windows(WindowEvent::Added(info)), 0);
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
                result: Ok(state_parked(open_size(), fullscreen, PlatformParking::Twin)),
            },
            0,
        );
        src.at(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(STREAM),
            },
            0,
        );
        src
    }
    fn at(&mut self, input: Input, now: u64) -> Vec<Output> {
        let mut out = Vec::new();
        self.e2.handle(&input, ms(now), &mut out);
        out
    }
    fn changed(&mut self, info: WindowInfo, now: u64) -> Vec<Output> {
        self.at(Input::Windows(WindowEvent::Changed(info)), now)
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
    fn request(&mut self, request: u32, size: PixelSize, now: u64) -> Vec<Output> {
        self.at(
            control(
                DST,
                Message::Resize {
                    fullscreen: false,
                    projection: ID,
                    request,
                    size,
                    scale: SCALE,
                },
            ),
            now,
        )
    }
    /// A source that has offered the window and is waiting for `Accepted`.
    fn offered() -> Self {
        let mut src = Self { e2: node() };
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
    fn tick(&mut self, now: u64) -> Vec<Output> {
        self.at(Input::Tick, now)
    }
    /// The next deadline, in ms.
    fn deadline(&self) -> Option<u64> {
        self.e2.next_deadline().map(|d| d.as_nanos() / 1_000_000)
    }
    /// Tick at every deadline up to and including `to` (ms), from `from` on, as the agent does,
    /// and return what each tick produced. A deadline that a tick doesn't clear never lets this
    /// finish.
    fn run(&mut self, from: u64, to: u64) -> Vec<(u64, Output)> {
        let mut seen = Vec::new();
        let mut now = from;
        for _ in 0..1_000 {
            let Some(deadline) = self.deadline() else {
                return seen;
            };
            let at = deadline.max(now);
            if at > to {
                return seen;
            }
            now = at;
            seen.extend(self.tick(at).into_iter().map(|o| (at, o)));
        }
        panic!("the deadline never advanced");
    }
    /// The window goes `state` at `frame` at `now`, the source re-parks it, and the platform
    /// finishes at `size` 10 ms later. Returns what the finish produced.
    fn go(
        &mut self,
        state: WindowState,
        frame: RectLogical,
        size: PixelSize,
        now: u64,
    ) -> Vec<Output> {
        let out = self.changed(window_in(state, frame), now);
        assert_eq!(reparks(&out), 1, "{state:?} at {now}: {out:?}");
        self.result(
            size,
            state == WindowState::Fullscreen,
            PlatformParking::Twin,
            now + 10,
        )
    }
}

#[test]
fn normal_to_fullscreen_is_one_re_park_and_the_geometry_follows_the_parked() {
    let mut src = Src::live();
    let out = src.changed(window_in(WindowState::Fullscreen, whole_frame()), 3_000);
    assert_eq!(out, vec![state_repark(open_size(), true)]);
    // While that runs, a further change waits for its result, whatever it is.
    let again = window_in(
        WindowState::Fullscreen,
        RectLogical::new(PointLogical::new(0.0, 30.0), SizeLogical::new(855.0, 673.0)),
    );
    assert!(src.changed(again, 3_005).is_empty());
    // The platform answers with the whole display: the capture crop follows it, then the
    // geometry the destination grows its proxy to.
    let out = src.result(whole(), true, PlatformParking::Twin, 3_010);
    assert_eq!(
        out,
        vec![
            Output::SetCaptureCrop {
                stream: STREAM,
                crop: Some(parked(whole()).content),
            },
            sent(DST, state_geometry(whole(), 0, Some(true))),
        ]
    );
}

#[test]
fn fullscreen_back_to_normal_is_a_re_park_again() {
    let mut src = Src::live();
    src.go(WindowState::Fullscreen, whole_frame(), whole(), 3_000);
    // Esc, after the gap: the window is back at its frame; the park goes back to the size the
    // destination last asked for.
    let out = src.changed(window_in(WindowState::Normal, normal_frame()), 5_100);
    assert_eq!(out, vec![resize_parked(open_size(), SCALE)]);
    let out = src.parked(open_size(), 5_110);
    assert_eq!(
        out,
        vec![
            Output::SetCaptureCrop {
                stream: STREAM,
                crop: Some(parked(open_size()).content),
            },
            sent(DST, geometry(open_size(), 0)),
        ]
    );
    // And it settles: the recorded state is Normal again.
    let mut renamed = window();
    renamed.title = "renamed".into();
    assert_eq!(reparks(&src.changed(renamed, 9_000)), 0);
}

#[test]
fn a_state_change_at_an_unchanged_frame_is_a_re_park() {
    // A window that goes off-Space keeps the frame it had: the state alone is the trigger, both
    // ways, for every state.
    for state in [
        WindowState::Hidden,
        WindowState::Fullscreen,
        WindowState::Minimized,
    ] {
        let mut src = Src::live();
        let out = src.changed(window_in(state, normal_frame()), 3_000);
        let mut expected = vec![state_repark(open_size(), state == WindowState::Fullscreen)];
        if state == WindowState::Hidden {
            expected.push(Output::StopCapture { stream: STREAM });
        }
        assert_eq!(out, expected, "{state:?}");
        src.result(
            open_size(),
            state == WindowState::Fullscreen,
            PlatformParking::Twin,
            3_010,
        );
        let out = src.changed(window(), 5_100);
        assert_eq!(out, vec![resize_parked(open_size(), SCALE)], "{state:?}");
        if state == WindowState::Hidden {
            assert!(
                src.result(open_size(), false, PlatformParking::Twin, 5_110)
                    .iter()
                    .any(|o| matches!(
                        o,
                        Output::StartCapture {
                            target: CaptureTarget::Display(DISPLAY),
                            ..
                        }
                    ))
            );
        }
    }
}

#[test]
fn frame_moves_while_not_normal_do_not_re_park() {
    for (state, first) in [
        (WindowState::Fullscreen, whole_frame()),
        (
            WindowState::Hidden,
            RectLogical::new(
                PointLogical::new(-30_000.0, 0.0),
                SizeLogical::new(320.25, 240.25),
            ),
        ),
    ] {
        let mut src = Src::live();
        // Not Normal now: one re-park for the state change, answered by the platform.
        src.go(state, first, whole(), 3_000);
        // The platform reports frames that wander (the display's, a menu bar's worth off, an
        // off-screen one): none is a trigger, however long after the last re-park.
        for (i, (x, y)) in [(0.0, 30.0), (12.0, 0.0), (-30_000.0, 500.0)]
            .into_iter()
            .enumerate()
        {
            let moved = window_in(
                state,
                RectLogical::new(PointLogical::new(x, y), SizeLogical::new(855.0, 673.0)),
            );
            let out = src.changed(moved, 10_000 + 10_000 * i as u64);
            assert_eq!(reparks(&out), 0, "{state:?} moved to ({x}, {y}): {out:?}");
        }
        // The state change back is.
        let out = src.changed(window(), 60_000);
        assert_eq!(out, vec![resize_parked(open_size(), SCALE)]);
    }
}

#[test]
fn a_window_projected_while_fullscreen_records_that_state_at_its_first_park() {
    let mut src = Src::live_in(window_in(WindowState::Fullscreen, whole_frame()));
    // A move is not a trigger, a state change out of Fullscreen is.
    let moved = window_in(
        WindowState::Fullscreen,
        RectLogical::new(PointLogical::new(0.0, 30.0), SizeLogical::new(855.0, 673.0)),
    );
    assert_eq!(reparks(&src.changed(moved, 3_000)), 0);
    assert_eq!(
        src.changed(window(), 3_001),
        vec![resize_parked(open_size(), SCALE)]
    );
}

#[test]
fn a_move_while_normal_still_re_parks_and_an_unchanged_state_is_no_trigger() {
    let mut src = Src::live();
    let mut moved = window();
    moved.frame = RectLogical::new(
        PointLogical::new(0.0, 26.0),
        SizeLogical::new(320.25, 214.25),
    );
    let out = src.changed(moved.clone(), 3_000);
    assert_eq!(out, vec![resize_parked(open_size(), SCALE)]);
    src.parked(open_size(), 3_010);
    // The same state and frame again (a title change, say) is nothing.
    moved.title = "renamed".into();
    assert_eq!(reparks(&src.changed(moved, 9_000)), 0);
}

#[test]
fn the_repark_gap_bounds_state_re_parks() {
    let mut src = Src::live();
    src.go(WindowState::Fullscreen, whole_frame(), whole(), 3_000);
    // Esc 0.5 s later: inside the gap, so no re-park, and none until 2 s after the last one.
    assert_eq!(reparks(&src.changed(window(), 3_500)), 0);
    let mut renamed = window();
    renamed.title = "renamed".into();
    assert_eq!(reparks(&src.changed(renamed.clone(), 4_999)), 0);
    // The recorded state is still Fullscreen, so the next report of the window after the gap
    // gets it re-parked, whatever else changed.
    renamed.title = "renamed again".into();
    let out = src.changed(renamed, 5_000);
    assert_eq!(reparks(&out), 1, "{out:?}");
    assert!(out.contains(&resize_parked(open_size(), SCALE)), "{out:?}");
    // Once parked, the churn is over.
    src.parked(open_size(), 5_010);
    assert_eq!(reparks(&src.changed(window(), 5_020)), 0);
    // And a quick toggle again is bounded the same way: 2 s from the re-park at 5_000.
    assert_eq!(
        reparks(&src.changed(window_in(WindowState::Fullscreen, whole_frame()), 6_999)),
        0
    );
    assert_eq!(
        reparks(&src.changed(window_in(WindowState::Fullscreen, whole_frame()), 7_000)),
        1
    );
}

#[test]
fn a_state_re_park_answers_nothing_and_carries_the_current_answered() {
    let mut src = Src::live();
    let a = px(800, 600);
    src.request(1, a, 10);
    assert_eq!(answers(&src.parked(a, 20)), vec![(1, a)]);
    // The window goes fullscreen by itself: a re-park at the size last asked for, with request 2
    // arriving while it runs. The re-park answers no request; request 2 waits behind it.
    let out = src.changed(window_in(WindowState::Fullscreen, whole_frame()), 3_000);
    assert_eq!(out, vec![state_repark(a, true)]);
    assert!(src.request(2, px(900, 700), 3_001).is_empty());
    let out = src.result(whole(), true, PlatformParking::Twin, 3_010);
    // The geometry says "answers 1": it reflects request 1 and nothing newer. Then request 2's
    // own resize starts.
    assert_eq!(answers(&out), vec![(1, whole())]);
    assert_eq!(out.last(), Some(&resize_parked(px(900, 700), SCALE)));
    // Request 2's result is what answers it.
    assert_eq!(
        answers(&src.parked(px(900, 700), 3_050)),
        vec![(2, px(900, 700))]
    );
}

/// A source that only lists windows, with every state in its list.
#[test]
fn hidden_windows_are_absent_from_browse() {
    let mut e2 = node();
    let mut sink = Vec::new();
    let states = [
        (1, WindowState::Normal, WindowRole::Toplevel),
        (2, WindowState::Hidden, WindowRole::Toplevel),
        (3, WindowState::Fullscreen, WindowRole::Toplevel),
        (4, WindowState::Minimized, WindowRole::Toplevel),
        (5, WindowState::Hidden, WindowRole::Dialog),
        (6, WindowState::Normal, WindowRole::Dialog),
    ];
    for (id, state, role) in states {
        let mut info = window_in(state, normal_frame());
        info.id = WindowId(id);
        info.role = role;
        e2.handle(&Input::Windows(WindowEvent::Added(info)), ms(0), &mut sink);
    }
    let listed = |e2: &mut E2, request: u32| -> Vec<u64> {
        let mut out = Vec::new();
        e2.handle(
            &control(DST, Message::ListWindows { request }),
            ms(1),
            &mut out,
        );
        match messages(&out).as_slice() {
            [Message::WindowList { windows, .. }] => windows.iter().map(|w| w.window.0).collect(),
            other => panic!("expected one WindowList, got {other:?}"),
        }
    };
    assert_eq!(listed(&mut e2, 1), vec![1, 3, 4, 6]);
    // A window that goes Hidden leaves the list, and comes back when it doesn't.
    let mut gone = window_in(WindowState::Hidden, normal_frame());
    gone.id = WindowId(1);
    e2.handle(
        &Input::Windows(WindowEvent::Changed(gone)),
        ms(2),
        &mut sink,
    );
    assert_eq!(listed(&mut e2, 2), vec![3, 4, 6]);
    let mut back = window_in(WindowState::Normal, normal_frame());
    back.id = WindowId(2);
    e2.handle(
        &Input::Windows(WindowEvent::Changed(back)),
        ms(3),
        &mut sink,
    );
    assert_eq!(listed(&mut e2, 3), vec![2, 3, 4, 6]);
}

// ---- Deferred, not dropped ----

/// What a finished park at `size` produces on a live projection: the capture crop, then the
/// geometry the destination resizes its proxy to.
fn finished(size: PixelSize) -> Vec<Output> {
    vec![
        Output::SetCaptureCrop {
            stream: STREAM,
            crop: Some(parked(size).content),
        },
        sent(DST, geometry(size, 0)),
    ]
}

#[test]
fn a_fullscreen_change_that_arrives_during_a_park_is_followed_with_no_further_event() {
    // The Mac's Safari: the frame changes on the way to fullscreen (a re-park), the state
    // change arrives while that park runs, the park finishes with the old geometry, and the
    // window source reports nothing more.
    let mut src = Src::live();
    let growing = RectLogical::new(
        PointLogical::new(0.0, 30.0),
        SizeLogical::new(320.25, 210.25),
    );
    let out = src.changed(window_in(WindowState::Normal, growing), 3_000);
    assert_eq!(out, vec![resize_parked(open_size(), SCALE)]);
    // Reported while it runs: nothing to act on yet.
    let out = src.changed(window_in(WindowState::Fullscreen, whole_frame()), 3_100);
    assert!(out.is_empty(), "{out:?}");
    // The park ends with the old geometry. It is sent, the stale park is noticed, and the gap
    // (2 s from 3 000) has not passed: the re-park is due at its end.
    assert_eq!(src.parked(open_size(), 3_200), finished(open_size()));
    assert_eq!(src.deadline(), Some(5_000));
    // At the gap's end, with no window event: a second re-park...
    assert_eq!(
        src.run(3_200, 10_000),
        vec![(5_000, state_repark(open_size(), true))]
    );
    // ...whose result is the whole display, and that is the end of it.
    assert_eq!(
        src.result(whole(), true, PlatformParking::Twin, 5_010),
        vec![
            Output::SetCaptureCrop {
                stream: STREAM,
                crop: Some(parked(whole()).content)
            },
            sent(DST, state_geometry(whole(), 0, Some(true))),
        ]
    );
    assert!(src.run(5_010, 600_000).is_empty());
    assert_eq!(src.deadline(), None);
}

#[test]
fn esc_within_the_gap_re_parks_once_at_the_gaps_end_with_no_further_event() {
    let mut src = Src::live();
    src.go(WindowState::Fullscreen, whole_frame(), whole(), 3_000);
    // Esc 0.5 s later: nothing now, and the same again later changes nothing.
    assert!(src.changed(window(), 3_500).is_empty());
    assert_eq!(reparks(&src.changed(window(), 4_000)), 0);
    assert_eq!(src.deadline(), Some(5_000));
    // Nothing before the gap's end...
    assert!(src.run(4_000, 4_999).is_empty());
    // ...then exactly one re-park, with nothing else needed to cause it.
    assert_eq!(
        src.run(4_999, 10_000),
        vec![(5_000, resize_parked(open_size(), SCALE))]
    );
    assert_eq!(src.parked(open_size(), 5_010), finished(open_size()));
    assert!(src.run(5_010, 600_000).is_empty());
    assert_eq!(src.deadline(), None);
}

#[test]
fn a_flip_during_a_park_that_flips_back_before_it_ends_causes_no_re_park() {
    let mut src = Src::live();
    let out = src.changed(window_in(WindowState::Fullscreen, whole_frame()), 3_000);
    assert_eq!(out, vec![state_repark(open_size(), true)]);
    // Out and back while it runs: the window is what the park was based on when it ends.
    assert!(src.changed(window(), 3_100).is_empty());
    assert!(
        src.changed(window_in(WindowState::Fullscreen, whole_frame()), 3_200)
            .is_empty()
    );
    assert_eq!(
        src.result(whole(), true, PlatformParking::Twin, 3_300),
        vec![
            Output::SetCaptureCrop {
                stream: STREAM,
                crop: Some(parked(whole()).content)
            },
            sent(DST, state_geometry(whole(), 0, Some(true))),
        ]
    );
    assert_eq!(src.deadline(), None);
    assert!(src.run(3_300, 600_000).is_empty());
}

#[test]
fn parking_that_un_fullscreens_the_window_costs_one_extra_re_park_and_then_it_is_stable() {
    // Hyprland: the park itself takes the window out of fullscreen.
    let mut src = Src::live();
    let mut count = 0;
    let out = src.changed(window_in(WindowState::Fullscreen, whole_frame()), 3_000);
    count += reparks(&out);
    assert_eq!(count, 1);
    // The platform un-fullscreens it while parking.
    assert!(src.changed(window(), 3_005).is_empty());
    assert_eq!(src.parked(open_size(), 3_010), finished(open_size()));
    // One more park, at the gap's end, based on the window as it is now.
    let seen = src.run(3_010, 10_000);
    assert_eq!(seen, vec![(5_000, resize_parked(open_size(), SCALE))]);
    count += seen.len();
    assert_eq!(src.parked(open_size(), 5_010), finished(open_size()));
    // Stable: no deadline, no re-park from any later report of the same window.
    assert_eq!(src.deadline(), None);
    let mut renamed = window();
    renamed.title = "renamed".into();
    count += reparks(&src.changed(renamed, 20_000));
    count += src.run(20_000, 600_000).len();
    assert_eq!(count, 2);
}

#[test]
fn ending_the_projection_during_a_pending_deferred_re_park_emits_nothing_afterwards() {
    let ends = [
        (
            "returned",
            Input::Command(Command::Return(ProjectionKey {
                source: SRC,
                projection: ID,
            })),
        ),
        (
            "closed by the destination",
            control(
                DST,
                Message::Close {
                    projection: ID,
                    reason: Reason::Returned,
                },
            ),
        ),
        (
            "window closed",
            Input::Windows(WindowEvent::Removed(WINDOW)),
        ),
    ];
    for (what, end) in ends {
        let mut src = Src::live();
        src.go(WindowState::Fullscreen, whole_frame(), whole(), 3_000);
        assert!(src.changed(window(), 3_500).is_empty(), "{what}");
        assert_eq!(src.deadline(), Some(5_000), "{what}");
        let out = src.at(end, 3_600);
        assert!(
            out.iter().any(|o| matches!(o, Output::Restore { .. })),
            "{what}: {out:?}"
        );
        // The gap ends, and nothing follows: no re-park, no geometry, no deadline.
        let later = src.run(3_600, 600_000);
        assert!(later.is_empty(), "{what}: {later:?}");
        let at_the_gaps_end = src.tick(5_000);
        assert!(at_the_gaps_end.is_empty(), "{what}: {at_the_gaps_end:?}");
        assert_eq!(src.deadline(), None, "{what}");
    }
}

#[test]
fn a_link_drop_during_a_pending_deferred_re_park_does_not_re_park_while_suspended() {
    let mut src = Src::live();
    src.go(WindowState::Fullscreen, whole_frame(), whole(), 3_000);
    assert!(src.changed(window(), 3_500).is_empty());
    assert_eq!(src.deadline(), Some(5_000));
    src.at(
        Input::Link(LinkEvent::Closed {
            peer: DST,
            error: LinkError::Closed,
        }),
        3_600,
    );
    // The gap ends while the projection is suspended: nothing is parked, and the only deadline
    // left is the suspension's own.
    let out = src.tick(5_000);
    assert_eq!(reparks(&out), 0, "{out:?}");
    assert!(src.deadline().is_none_or(|d| d > 5_000));
}

#[test]
fn a_state_change_before_the_projection_is_live_is_followed_when_it_goes_live() {
    let mut src = Src::offered();
    src.at(
        control(
            DST,
            Message::Accepted {
                projection: ID,
                size: open_size(),
                scale: SCALE,
            },
        ),
        1,
    );
    // The window goes fullscreen while it is being parked: not a re-park (it isn't live), and
    // nothing more is reported afterwards.
    assert!(
        src.changed(window_in(WindowState::Fullscreen, whole_frame()), 5)
            .is_empty()
    );
    let out = src.parked(open_size(), 10);
    assert_eq!(reparks(&out), 0, "{out:?}");
    // Live: the park was based on a Normal window and it is Fullscreen now.
    let out = src.at(
        Input::CaptureStarted {
            projection: ID,
            result: Ok(STREAM),
        },
        20,
    );
    assert_eq!(reparks(&out), 1, "{out:?}");
    assert_eq!(
        src.result(whole(), true, PlatformParking::Twin, 30),
        vec![
            Output::SetCaptureCrop {
                stream: STREAM,
                crop: Some(parked(whole()).content)
            },
            sent(DST, state_geometry(whole(), 0, Some(true))),
        ]
    );
    assert_eq!(src.deadline(), None);
}

// FS2: complete fake source/destination exchanges. Neither role uses a native adapter.
fn key() -> ProjectionKey {
    ProjectionKey {
        source: SRC,
        projection: ID,
    }
}
fn state_geometry(size: PixelSize, answers: u32, fullscreen: Option<bool>) -> Message {
    Message::Geometry {
        projection: ID,
        size,
        parking: ParkingKind::Twin,
        answers,
        fullscreen,
    }
}
fn state_resize(request: u32, size: PixelSize, fullscreen: bool) -> Message {
    Message::Resize {
        projection: ID,
        request,
        size,
        scale: SCALE,
        fullscreen,
    }
}
fn state_parked(size: PixelSize, fullscreen: bool, kind: PlatformParking) -> Parked {
    Parked {
        fullscreen,
        kind,
        ..parked(size)
    }
}
fn state_repark(size: PixelSize, fullscreen: bool) -> Output {
    Output::ResizeParked {
        window: WINDOW,
        size,
        scale: SCALE,
        fullscreen,
    }
}
impl Src {
    fn with_parking(kind: PlatformParking, fullscreen: bool) -> Self {
        let mut src = Self::offered();
        let mut info = window();
        info.pid = Some(42);
        src.changed(info, 0);
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
        src.result(open_size(), fullscreen, kind, 0);
        src.at(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(STREAM),
            },
            0,
        );
        src
    }
    fn result(
        &mut self,
        size: PixelSize,
        fullscreen: bool,
        kind: PlatformParking,
        now: u64,
    ) -> Vec<Output> {
        self.at(
            Input::Parked {
                window: WINDOW,
                result: Ok(state_parked(size, fullscreen, kind)),
            },
            now,
        )
    }
    fn toggle(&mut self, state: WindowState, now: u64) -> Vec<Output> {
        let mut info = window_in(
            state,
            if state == WindowState::Fullscreen {
                whole_frame()
            } else {
                normal_frame()
            },
        );
        info.pid = Some(42);
        self.changed(info, now)
    }
}
struct Dst {
    e2: E2,
}
impl Dst {
    fn new() -> Self {
        let (mut e2, out) = E2::new(
            &EngineConfig::new(DST),
            Box::new(MemoryJournal::default()),
            ms(0),
        )
        .unwrap();
        assert!(out.is_empty());
        let mut out = Vec::new();
        e2.handle(&Input::Session(SessionEvent::State(OPEN)), ms(0), &mut out);
        e2.handle(&Input::PeerUp { peer: SRC }, ms(0), &mut out);
        e2.handle(
            &Input::Grants([(SRC, [Capability::WindowPresent].into())].into()),
            ms(0),
            &mut out,
        );
        let mut dst = Self { e2 };
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
                key: key(),
                result: Ok((open_size(), SCALE)),
            },
            0,
        );
        assert!(messages(&accepted).contains(&Message::Accepted {
            projection: ID,
            size: open_size(),
            scale: SCALE
        }));
        dst.geometry(open_size(), 0, Some(false), 0);
        dst
    }
    fn at(&mut self, input: Input, now: u64) -> Vec<Output> {
        let mut out = Vec::new();
        self.e2.handle(&input, ms(now), &mut out);
        out
    }
    fn host(&mut self, event: ProxyEvent, now: u64) -> Vec<Output> {
        self.at(Input::Proxy { key: key(), event }, now)
    }
    fn resized(&mut self, size: PixelSize, now: u64) -> Vec<Output> {
        self.host(ProxyEvent::Resized { size, scale: SCALE }, now)
    }
    fn geometry(
        &mut self,
        size: PixelSize,
        answers: u32,
        fullscreen: Option<bool>,
        now: u64,
    ) -> Vec<Output> {
        self.at(control(SRC, state_geometry(size, answers, fullscreen)), now)
    }
    fn tick(&mut self, now: u64) -> Vec<Output> {
        self.at(Input::Tick, now)
    }
}
fn proxy_states(out: &[Output]) -> Vec<bool> {
    out.iter()
        .filter_map(|o| match o {
            Output::ProxyFullscreen { fullscreen, .. } => Some(*fullscreen),
            _ => None,
        })
        .collect()
}
fn no_end(out: &[Output]) {
    assert!(
        !out.iter()
            .any(|o| matches!(o, Output::Restore { .. } | Output::StopCapture { .. })),
        "{out:?}"
    );
    assert!(
        !messages(out)
            .iter()
            .any(|m| matches!(m, Message::End { .. })),
        "{out:?}"
    );
}
#[test]
fn app_fullscreen_sends_geometry_and_proxy_goes_fullscreen() {
    let mut src = Src::with_parking(PlatformParking::Twin, false);
    let mut dst = Dst::new();
    assert_eq!(
        src.toggle(WindowState::Fullscreen, 3_000),
        vec![state_repark(open_size(), true)]
    );
    let out = src.result(whole(), true, PlatformParking::Twin, 3_010);
    assert_eq!(
        out,
        vec![
            Output::SetCaptureCrop {
                stream: STREAM,
                crop: Some(parked(whole()).content)
            },
            sent(DST, state_geometry(whole(), 0, Some(true))),
        ]
    );
    let command = dst.geometry(whole(), 0, Some(true), 3_010);
    assert_eq!(
        command,
        vec![Output::ProxyFullscreen {
            key: key(),
            fullscreen: true
        }]
    );
    assert!(dst.host(ProxyEvent::Fullscreen(true), 3_020).is_empty());
    assert_eq!(
        messages(&dst.resized(whole(), 3_021)),
        vec![state_resize(1, whole(), true)]
    );
    assert_eq!(
        src.at(control(DST, state_resize(1, whole(), true)), 3_022),
        vec![sent(DST, state_geometry(whole(), 1, Some(true)))]
    );
    assert!(dst.geometry(whole(), 1, Some(true), 3_023).is_empty());
    assert!(proxy_states(&dst.tick(3_271)).is_empty());
}
#[test]
fn stand_in_same_pid_on_parked_display() {
    for kind in [PlatformParking::Twin, PlatformParking::Mirror] {
        let mut src = Src::with_parking(kind, false);
        let mut stand_in = window_in(WindowState::Fullscreen, whole_frame());
        stand_in.id = WindowId(11);
        stand_in.pid = Some(42);
        no_end(&src.at(Input::Windows(WindowEvent::Added(stand_in)), 3_000));
        let out = src.toggle(WindowState::Hidden, 3_500);
        assert!(
            out.contains(&state_repark(open_size(), true)),
            "{kind:?}: {out:?}"
        );
        let out = src.result(whole(), true, kind, 3_510);
        if kind == PlatformParking::Twin {
            assert!(
                out.contains(&Output::SetCaptureCrop {
                    stream: STREAM,
                    crop: Some(parked(whole()).content)
                }),
                "{out:?}"
            );
            assert!(
                !out.iter()
                    .any(|o| matches!(o, Output::StartCapture { .. } | Output::StopCapture { .. }))
            );
        } else {
            let mut trace = out;
            assert!(
                trace.iter().any(|o| matches!(
                    o,
                    Output::StartCapture {
                        target: crosspane_platform::CaptureTarget::Window(WindowId(11)),
                        ..
                    }
                )),
                "{trace:?}"
            );
            trace.extend(src.at(
                Input::CaptureStarted {
                    projection: ID,
                    result: Ok(StreamId(2)),
                },
                3_520,
            ));
            let start = trace
                .iter()
                .position(|o| matches!(o, Output::StartCapture { .. }))
                .unwrap();
            let stop = trace
                .iter()
                .position(|o| matches!(o, Output::StopCapture { stream: STREAM }))
                .unwrap();
            assert!(start < stop, "{trace:?}");
        }
        projection_survives(&src.at(Input::Windows(WindowEvent::Removed(WindowId(11))), 5_500));
        assert_eq!(
            src.toggle(WindowState::Normal, 5_600),
            vec![state_repark(open_size(), false)]
        );
    }
}
#[test]
fn stand_in_requires_known_pid_grace_and_parked_display() {
    for (pid, delay, display, expected) in [
        (Some(7), 10, DISPLAY, false),
        (None, 10, DISPLAY, false),
        (Some(42), 1_001, DISPLAY, false),
        (Some(42), 1_000, DISPLAY, true),
        (Some(42), 10, DisplayId(8), false),
    ] {
        let mut src = Src::with_parking(PlatformParking::Twin, false);
        projection_survives(&src.toggle(WindowState::Hidden, 3_000));
        projection_survives(&src.result(open_size(), false, PlatformParking::Twin, 3_001));
        let mut candidate = window_in(WindowState::Fullscreen, whole_frame());
        candidate.id = WindowId(11);
        candidate.pid = pid;
        candidate.display = Some(display);
        let mut out = src.at(Input::Windows(WindowEvent::Added(candidate)), 3_000 + delay);
        out.extend(src.tick(5_000));
        assert_eq!(
            out.contains(&state_repark(open_size(), true)),
            expected,
            "{out:?}"
        );
        if expected {
            out.extend(src.result(whole(), true, PlatformParking::Twin, 5_010));
            assert!(
                out.iter().any(|o| matches!(
                    o,
                    Output::StartCapture {
                        target: CaptureTarget::Display(DISPLAY),
                        ..
                    }
                )),
                "{out:?}"
            );
            assert!(
                messages(&out).iter().any(|m| matches!(
                    m,
                    Message::Geometry {
                        parking: ParkingKind::Twin,
                        fullscreen: Some(true),
                        ..
                    }
                )),
                "{out:?}"
            );
        }
    }
}
#[test]
fn hidden_without_stand_in_never_ends() {
    let mut src = Src::with_parking(PlatformParking::Twin, false);
    projection_survives(&src.toggle(WindowState::Hidden, 3_000));
    no_end(&src.tick(4_001));
    no_end(&src.tick(30_000));
    let out = src.at(Input::Command(Command::Return(key())), 30_001);
    assert!(
        out.iter()
            .any(|o| matches!(o, Output::Restore { window: WINDOW, .. })),
        "{out:?}"
    );
}
#[test]
fn removed_still_ends_with_window_closed() {
    let mut src = Src::with_parking(PlatformParking::Twin, false);
    src.toggle(WindowState::Hidden, 3_000);
    let out = src.at(Input::Windows(WindowEvent::Removed(WINDOW)), 3_001);
    assert!(
        messages(&out).contains(&Message::End {
            projection: ID,
            reason: Reason::WindowClosed
        }),
        "{out:?}"
    );
}
#[test]
fn host_confirmation_is_not_a_request() {
    let mut dst = Dst::new();
    assert_eq!(
        proxy_states(&dst.geometry(whole(), 0, Some(true), 0)),
        vec![true]
    );
    assert!(dst.host(ProxyEvent::Fullscreen(true), 2_000).is_empty());
    assert!(messages(&dst.tick(2_050)).is_empty());
    // Its new monitor size still renegotiates: fullscreen confirmation only consumes the flag.
    assert_eq!(
        messages(&dst.resized(whole(), 2_051)),
        vec![state_resize(1, whole(), true)]
    );
}
#[test]
fn user_toggle_sends_numbered_resize_with_flag() {
    let mut dst = Dst::new();
    assert!(dst.host(ProxyEvent::Fullscreen(true), 10).is_empty());
    assert!(messages(&dst.tick(59)).is_empty());
    assert_eq!(
        messages(&dst.tick(60)),
        vec![state_resize(1, open_size(), true)]
    );
    assert!(dst.host(ProxyEvent::Fullscreen(false), 61).is_empty());
    assert!(messages(&dst.resized(open_size(), 62)).is_empty());
    assert_eq!(
        messages(&dst.tick(110)),
        vec![state_resize(2, open_size(), false)]
    );
    assert!(messages(&dst.tick(111)).is_empty());
}
#[test]
fn stale_answer_never_toggles() {
    let mut dst = Dst::new();
    assert_eq!(
        messages(&dst.resized(whole(), 0)),
        vec![state_resize(1, whole(), false)]
    );
    assert!(dst.geometry(whole(), 0, Some(true), 500).is_empty());
    assert_eq!(
        proxy_states(&dst.geometry(whole(), 1, Some(true), 501)),
        vec![true]
    );
}
#[test]
fn old_source_zero_never_unfullscreens() {
    let mut dst = Dst::new();
    dst.host(ProxyEvent::Fullscreen(true), 0);
    assert_eq!(
        messages(&dst.resized(whole(), 1)),
        vec![state_resize(1, whole(), true)]
    );
    assert!(dst.geometry(open_size(), 0, None, 500).is_empty());
    assert!(dst.geometry(open_size(), 1, None, 501).is_empty());
}
#[test]
fn refused_fullscreen_snaps_proxy_back() {
    let mut src = Src::with_parking(PlatformParking::Twin, false);
    let mut dst = Dst::new();
    dst.host(ProxyEvent::Fullscreen(true), 0);
    assert_eq!(
        messages(&dst.resized(whole(), 1)),
        vec![state_resize(1, whole(), true)]
    );
    assert_eq!(
        src.at(control(DST, state_resize(1, whole(), true)), 2),
        vec![state_repark(whole(), true)]
    );
    let out = src.result(whole(), false, PlatformParking::Twin, 10);
    assert!(
        messages(&out).contains(&state_geometry(whole(), 1, Some(false))),
        "{out:?}"
    );
    assert_eq!(
        proxy_states(&dst.geometry(whole(), 1, Some(false), 251)),
        vec![false]
    );
    no_end(&out);
}
#[test]
fn queued_state_change_while_resizing() {
    let mut src = Src::with_parking(PlatformParking::Twin, false);
    assert_eq!(
        src.at(control(DST, state_resize(1, px(900, 700), false)), 3_000),
        vec![state_repark(px(900, 700), false)]
    );
    assert!(src.toggle(WindowState::Fullscreen, 3_001).is_empty());
    let out = src.result(px(900, 700), false, PlatformParking::Twin, 3_010);
    assert_eq!(
        messages(&out),
        vec![state_geometry(px(900, 700), 1, Some(false))]
    );
    assert!(out.contains(&state_repark(px(900, 700), true)), "{out:?}");
    // The most recent destination request supersedes a queued request's state and carries its own answer.
    assert!(
        src.at(control(DST, state_resize(2, whole(), true)), 3_011)
            .is_empty()
    );
    assert!(
        src.at(control(DST, state_resize(3, open_size(), false)), 3_012)
            .is_empty()
    );
    let out = src.result(whole(), true, PlatformParking::Twin, 3_020);
    assert_eq!(messages(&out), vec![state_geometry(whole(), 1, Some(true))]);
    assert!(out.contains(&state_repark(open_size(), false)), "{out:?}");
    assert_eq!(
        messages(&src.result(open_size(), false, PlatformParking::Twin, 3_030)),
        vec![state_geometry(open_size(), 3, Some(false))]
    );
}
#[test]
fn no_repark_while_fullscreen_frame_moves() {
    let mut src = Src::with_parking(PlatformParking::Twin, false);
    src.toggle(WindowState::Fullscreen, 3_000);
    src.result(whole(), true, PlatformParking::Twin, 3_010);
    for now in [5_000, 8_000, 12_000] {
        let mut info = window_in(WindowState::Fullscreen, normal_frame());
        info.pid = Some(42);
        info.frame.origin.y = now as f64;
        assert!(src.changed(info, now).is_empty());
        assert!(src.tick(now + 1).is_empty());
    }
}
#[test]
fn resume_resize_carries_flag() {
    let mut dst = Dst::new();
    dst.host(ProxyEvent::Fullscreen(true), 0);
    dst.resized(whole(), 1);
    dst.geometry(whole(), 1, Some(true), 300);
    dst.at(
        Input::Link(LinkEvent::Closed {
            peer: SRC,
            error: LinkError::Closed,
        }),
        400,
    );
    dst.at(Input::PeerUp { peer: SRC }, 500);
    let out = dst.at(
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
        501,
    );
    assert_eq!(
        messages(&out),
        vec![
            Message::Accepted {
                projection: ID,
                size: whole(),
                scale: SCALE
            },
            state_resize(2, whole(), true),
        ]
    );
}

#[test]
fn flag_only_resize_is_work_and_parked_refusal_is_authoritative() {
    let mut src = Src::with_parking(PlatformParking::Twin, false);
    assert_eq!(
        src.at(control(DST, state_resize(1, open_size(), true)), 10),
        vec![state_repark(open_size(), true)]
    );
    let out = src.result(open_size(), false, PlatformParking::Twin, 20);
    assert_eq!(
        messages(&out),
        vec![state_geometry(open_size(), 1, Some(false))]
    );
    assert_eq!(
        src.at(control(DST, state_resize(2, open_size(), true)), 30),
        vec![state_repark(open_size(), true)]
    );
    assert_eq!(
        messages(&src.result(open_size(), true, PlatformParking::Twin, 40)),
        vec![state_geometry(open_size(), 2, Some(true))]
    );
    assert_eq!(
        src.at(control(DST, state_resize(3, open_size(), true)), 50),
        vec![sent(DST, state_geometry(open_size(), 3, Some(true)))]
    );
}
#[test]
fn queued_request_keeps_latest_flag_and_number() {
    let mut src = Src::with_parking(PlatformParking::Twin, false);
    src.at(control(DST, state_resize(1, px(800, 600), false)), 10);
    assert!(
        src.at(control(DST, state_resize(2, px(900, 700), false)), 11)
            .is_empty()
    );
    assert!(
        src.at(control(DST, state_resize(3, px(900, 700), true)), 12)
            .is_empty()
    );
    let out = src.result(px(800, 600), false, PlatformParking::Twin, 20);
    assert_eq!(
        messages(&out),
        vec![state_geometry(px(800, 600), 1, Some(false))]
    );
    assert_eq!(out.last(), Some(&state_repark(px(900, 700), true)));
    assert_eq!(
        messages(&src.result(px(900, 700), true, PlatformParking::Twin, 30)),
        vec![state_geometry(px(900, 700), 3, Some(true))]
    );
    assert!(
        src.at(control(DST, state_resize(2, open_size(), false)), 31)
            .is_empty()
    );
}
#[test]
fn fullscreen_answers_wait_for_user_quiet_and_supersession() {
    let mut dst = Dst::new();
    dst.host(ProxyEvent::Fullscreen(true), 0);
    dst.resized(whole(), 1);
    assert!(dst.geometry(open_size(), 1, Some(false), 10).is_empty());
    assert_eq!(
        messages(&dst.resized(px(1900, 1080), 60)),
        vec![state_resize(2, px(1900, 1080), true)]
    );
    assert!(
        proxy_states(&dst.tick(251)).is_empty(),
        "held stale answer must be cleared"
    );
    assert!(dst.geometry(open_size(), 1, Some(false), 252).is_empty());
    assert!(dst.geometry(open_size(), 2, Some(false), 253).is_empty());
    assert!(proxy_states(&dst.tick(309)).is_empty());
    assert_eq!(proxy_states(&dst.tick(310)), vec![false]);
}
#[test]
fn stand_in_replacement_is_serialized_and_late_start_is_stopped() {
    let mut src = Src::with_parking(PlatformParking::Mirror, false);
    let mut candidate = window_in(WindowState::Fullscreen, whole_frame());
    candidate.id = WindowId(11);
    candidate.pid = Some(42);
    src.at(Input::Windows(WindowEvent::Added(candidate)), 3_000);
    src.toggle(WindowState::Hidden, 3_001);
    let out = src.result(whole(), true, PlatformParking::Mirror, 3_010);
    assert_eq!(
        out.iter()
            .filter(|o| matches!(o, Output::StartCapture { .. }))
            .count(),
        1
    );
    src.toggle(WindowState::Normal, 5_100);
    let out = src.result(open_size(), false, PlatformParking::Mirror, 5_110);
    assert!(
        !out.iter().any(|o| matches!(o, Output::StartCapture { .. })),
        "{out:?}"
    );
    let out = src.at(
        Input::CaptureStarted {
            projection: ID,
            result: Ok(StreamId(2)),
        },
        5_120,
    );
    assert_eq!(
        out,
        vec![
            Output::StopCapture { stream: STREAM },
            Output::StartCapture {
                projection: ID,
                peer: DST,
                target: crosspane_platform::CaptureTarget::Window(WINDOW),
                crop: None,
                max_fps: 60
            },
        ]
    );
    src.at(Input::Command(Command::Return(key())), 5_121);
    assert_eq!(
        src.at(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(StreamId(3))
            },
            5_122
        ),
        vec![Output::StopCapture {
            stream: StreamId(3)
        }]
    );
}

#[test]
fn programmatic_toggle_same_size_renegotiates_without_geometry_echo() {
    let mut dst = Dst::new();
    assert_eq!(
        dst.geometry(open_size(), 0, Some(true), 0),
        vec![Output::ProxyFullscreen {
            key: key(),
            fullscreen: true
        },]
    );
    assert!(dst.host(ProxyEvent::Fullscreen(true), 1).is_empty());
    assert_eq!(
        messages(&dst.resized(open_size(), 2)),
        vec![state_resize(1, open_size(), true)]
    );
    assert!(dst.geometry(open_size(), 1, Some(true), 3).is_empty());
    assert!(dst.tick(252).iter().all(|o| !matches!(
        o,
        Output::ProxyGeometry { .. } | Output::ProxyFullscreen { .. }
    )));
}

#[test]
fn user_cancels_unconfirmed_fullscreen_at_the_original_state() {
    for reports_state in [false, true] {
        let mut dst = Dst::new();
        assert_eq!(
            proxy_states(&dst.geometry(open_size(), 0, Some(true), 2)),
            vec![true]
        );
        if reports_state {
            assert!(dst.host(ProxyEvent::Fullscreen(false), 3).is_empty());
        }
        assert_eq!(
            messages(&dst.resized(open_size(), 3)),
            vec![state_resize(1, open_size(), false)]
        );
        assert!(dst.geometry(open_size(), 1, Some(false), 4).is_empty());
        assert!(dst.tick(253).iter().all(|o| !matches!(
            o,
            Output::ProxyGeometry { .. } | Output::ProxyFullscreen { .. }
        )));
    }
}

fn candidate(id: u64, display: DisplayId) -> WindowInfo {
    let mut info = window_in(WindowState::Fullscreen, whole_frame());
    info.id = WindowId(id);
    info.pid = Some(42);
    info.display = Some(display);
    info
}
fn projection_survives(out: &[Output]) {
    assert!(
        !out.iter().any(|o| matches!(o, Output::Restore { .. })),
        "{out:?}"
    );
    assert!(
        !messages(out)
            .iter()
            .any(|m| matches!(m, Message::End { .. })),
        "{out:?}"
    );
}
fn lost(src: &mut Src, stream: StreamId, now: u64) -> Vec<Output> {
    src.at(
        Input::CaptureEnded {
            stream,
            reason: StreamEndReason::TargetGone,
        },
        now,
    )
}
fn started(src: &mut Src, stream: StreamId, now: u64) -> Vec<Output> {
    src.at(
        Input::CaptureStarted {
            projection: ID,
            result: Ok(stream),
        },
        now,
    )
}

#[test]
fn parked_display_candidate_wins_in_both_event_orders() {
    for kind in [PlatformParking::Twin, PlatformParking::Mirror] {
        for foreign_first in [false, true] {
            let mut src = Src::with_parking(kind, false);
            let foreign = candidate(12, DisplayId(8));
            let local = candidate(11, DISPLAY);
            let mut trace = src.at(
                Input::Windows(WindowEvent::Added(if foreign_first {
                    foreign.clone()
                } else {
                    local.clone()
                })),
                2_990,
            );
            trace.extend(src.toggle(WindowState::Hidden, 3_000));
            trace.extend(src.at(
                Input::Windows(WindowEvent::Added(if foreign_first {
                    local
                } else {
                    foreign
                })),
                3_001,
            ));
            trace.extend(src.result(whole(), true, kind, 3_010));
            let target = if kind == PlatformParking::Twin {
                CaptureTarget::Display(DISPLAY)
            } else {
                CaptureTarget::Window(WindowId(11))
            };
            if kind == PlatformParking::Mirror {
                assert!(
                    trace.iter().any(
                        |o| matches!(o, Output::StartCapture { target: t, .. } if *t == target)
                    ),
                    "{trace:?}"
                );
            }
            assert!(
                !trace.iter().any(|o| matches!(
                    o,
                    Output::StartCapture {
                        target: CaptureTarget::Window(WindowId(12)),
                        ..
                    }
                )),
                "{trace:?}"
            );
            assert!(messages(&trace).iter().any(|m| matches!(m, Message::Geometry { parking, fullscreen: Some(true), .. } if *parking == if kind == PlatformParking::Twin { ParkingKind::Twin } else { ParkingKind::Mirror })), "{trace:?}");
        }
    }
}

#[test]
fn hidden_mirror_loss_during_entry_keeps_projection_and_cleans_stream() {
    let mut src = Src::with_parking(PlatformParking::Mirror, false);
    src.at(
        Input::Windows(WindowEvent::Added(candidate(11, DISPLAY))),
        2_990,
    );
    assert_eq!(
        src.toggle(WindowState::Hidden, 3_000),
        vec![state_repark(open_size(), true)]
    );
    let out = lost(&mut src, STREAM, 3_001);
    projection_survives(&out);
    assert_eq!(out, vec![Output::StopCapture { stream: STREAM }]);
    let out = src.result(whole(), true, PlatformParking::Mirror, 3_010);
    assert!(
        out.iter().any(|o| matches!(
            o,
            Output::StartCapture {
                target: CaptureTarget::Window(WindowId(11)),
                ..
            }
        )),
        "{out:?}"
    );
    assert!(started(&mut src, StreamId(2), 3_020).is_empty());
    let out = src.at(Input::Command(Command::Return(key())), 3_021);
    assert!(
        out.contains(&Output::StopCapture {
            stream: StreamId(2)
        }),
        "{out:?}"
    );
    assert!(
        !out.contains(&Output::StopCapture { stream: STREAM }),
        "{out:?}"
    );
}

#[test]
fn newer_geometry_cancels_unconfirmed_fullscreen_in_order() {
    let mut dst = Dst::new();
    let mut trace = dst.geometry(open_size(), 0, Some(true), 1);
    trace.extend(dst.geometry(open_size(), 0, Some(false), 2));
    assert_eq!(
        trace,
        vec![
            Output::ProxyFullscreen {
                key: key(),
                fullscreen: true
            },
            Output::ProxyFullscreen {
                key: key(),
                fullscreen: false
            },
        ]
    );
    assert!(dst.host(ProxyEvent::Fullscreen(false), 3).is_empty());
    assert!(dst.resized(open_size(), 4).is_empty());
    assert!(dst.tick(254).iter().all(|o| !matches!(
        o,
        Output::ProxyGeometry { .. } | Output::ProxyFullscreen { .. }
    ) && !matches!(
        o,
        Output::SendControl {
            msg: ControlMessage::Projection(Message::Resize { .. }),
            ..
        }
    )));
}

#[test]
fn pending_display_does_not_crop_or_classify_the_active_window_stream() {
    let mut src = Src::with_parking(PlatformParking::Mirror, false);
    src.at(
        Input::Windows(WindowEvent::Added(candidate(11, DISPLAY))),
        2_990,
    );
    src.toggle(WindowState::Hidden, 3_000);
    src.result(whole(), true, PlatformParking::Mirror, 3_010);
    started(&mut src, StreamId(2), 3_020);
    src.at(control(DST, state_resize(1, px(800, 600), true)), 3_030);
    let out = src.result(px(800, 600), true, PlatformParking::Twin, 3_040);
    assert!(
        out.iter().any(|o| matches!(
            o,
            Output::StartCapture {
                target: CaptureTarget::Display(DISPLAY),
                ..
            }
        )),
        "{out:?}"
    );
    assert!(
        !out.iter().any(|o| matches!(
            o,
            Output::SetCaptureCrop {
                stream: StreamId(2),
                ..
            }
        )),
        "{out:?}"
    );
    let out = lost(&mut src, StreamId(2), 3_041);
    projection_survives(&out);
    assert_eq!(
        out,
        vec![Output::StopCapture {
            stream: StreamId(2)
        }]
    );
    assert!(started(&mut src, StreamId(3), 3_042).is_empty());
    let out = src.at(Input::Command(Command::Return(key())), 3_043);
    assert!(
        out.contains(&Output::StopCapture {
            stream: StreamId(3)
        }),
        "{out:?}"
    );
}

#[test]
fn pending_stand_in_does_not_hide_loss_of_a_visible_original() {
    let mut src = Src::with_parking(PlatformParking::Mirror, false);
    src.at(
        Input::Windows(WindowEvent::Added(candidate(11, DISPLAY))),
        2_990,
    );
    src.toggle(WindowState::Hidden, 3_000);
    src.result(whole(), true, PlatformParking::Mirror, 3_010);
    src.toggle(WindowState::Normal, 5_100);
    let out = lost(&mut src, STREAM, 5_101);
    assert!(
        messages(&out).contains(&Message::End {
            projection: ID,
            reason: Reason::WindowClosed
        }),
        "{out:?}"
    );
    assert_eq!(
        started(&mut src, StreamId(2), 5_102),
        vec![Output::StopCapture {
            stream: StreamId(2)
        }]
    );
}

#[test]
fn repeated_stand_in_loss_cleans_each_stream_once_and_recovers() {
    let mut src = Src::with_parking(PlatformParking::Mirror, false);
    src.at(
        Input::Windows(WindowEvent::Added(candidate(11, DISPLAY))),
        2_990,
    );
    let mut trace = Vec::new();
    let mut stream = STREAM;
    for cycle in 0..3 {
        let now = 3_000 + cycle * 6_000;
        src.toggle(WindowState::Hidden, now);
        trace.extend(src.result(whole(), true, PlatformParking::Mirror, now + 10));
        let next = StreamId(2 + cycle * 2);
        trace.extend(started(&mut src, next, now + 20));
        stream = next;
        let out = lost(&mut src, stream, now + 21);
        projection_survives(&out);
        assert_eq!(out, vec![Output::StopCapture { stream }]);
        trace.extend(out);
        src.toggle(WindowState::Normal, now + 3_000);
        let out = src.result(open_size(), false, PlatformParking::Mirror, now + 3_010);
        assert!(
            out.iter().any(|o| matches!(
                o,
                Output::StartCapture {
                    target: CaptureTarget::Window(WINDOW),
                    ..
                }
            )),
            "{out:?}"
        );
        stream = StreamId(3 + cycle * 2);
        trace.extend(started(&mut src, stream, now + 3_020));
    }
    trace.extend(src.at(Input::Command(Command::Return(key())), 21_100));
    for id in 1..=7 {
        assert_eq!(
            trace
                .iter()
                .filter(|o| matches!(o, Output::StopCapture { stream } if *stream == StreamId(id)))
                .count(),
            1,
            "{trace:?}"
        );
    }
    assert_eq!(stream, StreamId(7));
}

#[test]
fn stand_in_leaving_parked_display_stops_frames_without_changing_parking() {
    for kind in [PlatformParking::Twin, PlatformParking::Mirror] {
        let mut src = Src::with_parking(kind, false);
        src.at(
            Input::Windows(WindowEvent::Added(candidate(11, DISPLAY))),
            2_990,
        );
        src.toggle(WindowState::Hidden, 3_000);
        src.result(whole(), true, kind, 3_010);
        let stream = if kind == PlatformParking::Mirror {
            started(&mut src, StreamId(2), 3_020);
            StreamId(2)
        } else {
            STREAM
        };
        let out = src.changed(candidate(11, DisplayId(8)), 3_030);
        projection_survives(&out);
        assert_eq!(out, vec![Output::StopCapture { stream }]);
        assert!(src.tick(4_100).is_empty());
        src.toggle(WindowState::Normal, 5_100);
        let out = src.result(open_size(), false, kind, 5_110);
        assert!(messages(&out).iter().any(|m| matches!(m, Message::Geometry { parking, .. } if *parking == if kind == PlatformParking::Twin { ParkingKind::Twin } else { ParkingKind::Mirror })), "{out:?}");
    }
}

#[test]
fn hidden_without_local_stand_in_stops_frames_and_resumes_capture() {
    for kind in [PlatformParking::Twin, PlatformParking::Mirror] {
        let mut src = Src::with_parking(kind, false);
        src.at(
            Input::Windows(WindowEvent::Added(candidate(11, DisplayId(8)))),
            2_990,
        );
        let out = src.toggle(WindowState::Hidden, 3_000);
        projection_survives(&out);
        assert!(
            out.contains(&Output::StopCapture { stream: STREAM }),
            "{out:?}"
        );
        assert!(lost(&mut src, STREAM, 3_001).is_empty());
        let out = src.result(open_size(), false, kind, 3_010);
        assert!(
            !out.iter().any(|o| matches!(o, Output::StartCapture { .. })),
            "{out:?}"
        );
        projection_survives(&src.tick(30_000));
        src.toggle(WindowState::Normal, 30_001);
        let out = src.result(open_size(), false, kind, 30_010);
        assert!(
            out.iter().any(|o| matches!(o, Output::StartCapture { .. })),
            "{out:?}"
        );
        assert!(started(&mut src, StreamId(2), 30_020).is_empty());
    }
}

#[test]
fn delayed_display_start_is_serialized_and_commits_the_latest_crop() {
    let mut src = Src::with_parking(PlatformParking::Mirror, false);
    src.at(
        Input::Windows(WindowEvent::Added(candidate(11, DISPLAY))),
        2_990,
    );
    src.toggle(WindowState::Hidden, 3_000);
    src.result(whole(), true, PlatformParking::Mirror, 3_010);
    started(&mut src, StreamId(2), 3_020);
    src.at(control(DST, state_resize(1, px(800, 600), true)), 3_030);
    let out = src.result(px(800, 600), true, PlatformParking::Twin, 3_040);
    assert!(
        out.contains(&Output::StartCapture {
            projection: ID,
            peer: DST,
            target: CaptureTarget::Display(DISPLAY),
            crop: Some(parked(px(800, 600)).content),
            max_fps: 60
        }),
        "{out:?}"
    );
    src.at(control(DST, state_resize(2, px(900, 700), true)), 3_050);
    let out = src.result(px(900, 700), true, PlatformParking::Twin, 3_060);
    assert!(
        !out.iter().any(|o| matches!(
            o,
            Output::StartCapture { .. } | Output::SetCaptureCrop { .. }
        )),
        "{out:?}"
    );
    assert_eq!(
        started(&mut src, StreamId(3), 3_070),
        vec![
            Output::StopCapture {
                stream: StreamId(2)
            },
            Output::SetCaptureCrop {
                stream: StreamId(3),
                crop: Some(parked(px(900, 700)).content)
            },
        ]
    );
    assert!(lost(&mut src, StreamId(2), 3_071).is_empty());
    let out = src.at(Input::Command(Command::Return(key())), 3_072);
    assert!(
        out.contains(&Output::StopCapture {
            stream: StreamId(3)
        }),
        "{out:?}"
    );
}

fn failed_start(src: &mut Src, now: u64) -> Vec<Output> {
    src.at(
        Input::CaptureStarted {
            projection: ID,
            result: Err(crosspane_engine::io::Failure::Other),
        },
        now,
    )
}

#[test]
fn obsolete_stand_in_start_failure_keeps_hidden_projection_and_resumes() {
    let mut src = Src::with_parking(PlatformParking::Mirror, false);
    src.at(
        Input::Windows(WindowEvent::Added(candidate(11, DISPLAY))),
        2_990,
    );
    src.toggle(WindowState::Hidden, 3_000);
    src.result(whole(), true, PlatformParking::Mirror, 3_010);
    assert_eq!(
        src.at(Input::Windows(WindowEvent::Removed(WindowId(11))), 3_011),
        vec![Output::StopCapture { stream: STREAM }]
    );
    assert!(failed_start(&mut src, 3_012).is_empty());
    projection_survives(&src.tick(30_000));
    src.toggle(WindowState::Normal, 30_001);
    let out = src.result(open_size(), false, PlatformParking::Mirror, 30_010);
    assert!(
        out.iter().any(|o| matches!(
            o,
            Output::StartCapture {
                target: CaptureTarget::Window(WINDOW),
                ..
            }
        )),
        "{out:?}"
    );
    assert!(started(&mut src, StreamId(2), 30_020).is_empty());
    assert!(
        src.at(Input::Command(Command::Return(key())), 30_021)
            .contains(&Output::StopCapture {
                stream: StreamId(2)
            })
    );
}

#[test]
fn obsolete_start_failure_starts_the_current_target() {
    let mut src = Src::with_parking(PlatformParking::Mirror, false);
    src.at(
        Input::Windows(WindowEvent::Added(candidate(11, DISPLAY))),
        2_990,
    );
    src.toggle(WindowState::Hidden, 3_000);
    projection_survives(&lost(&mut src, STREAM, 3_001));
    src.result(whole(), true, PlatformParking::Mirror, 3_010);
    src.toggle(WindowState::Normal, 5_100);
    src.result(open_size(), false, PlatformParking::Mirror, 5_110);
    let out = failed_start(&mut src, 5_120);
    assert_eq!(
        out,
        vec![Output::StartCapture {
            projection: ID,
            peer: DST,
            target: CaptureTarget::Window(WINDOW),
            crop: None,
            max_fps: 60
        }]
    );
    assert!(started(&mut src, StreamId(2), 5_121).is_empty());
}

#[test]
fn current_stand_in_start_failure_still_ends_projection() {
    let mut src = Src::with_parking(PlatformParking::Mirror, false);
    src.at(
        Input::Windows(WindowEvent::Added(candidate(11, DISPLAY))),
        2_990,
    );
    src.toggle(WindowState::Hidden, 3_000);
    src.result(whole(), true, PlatformParking::Mirror, 3_010);
    let out = failed_start(&mut src, 3_011);
    assert!(
        out.contains(&Output::StopCapture { stream: STREAM }),
        "{out:?}"
    );
    assert!(
        messages(&out).contains(&Message::End {
            projection: ID,
            reason: Reason::Failed
        }),
        "{out:?}"
    );
}

#[test]
fn coalesced_fullscreen_cancellation_applies_geometry_without_host_callbacks() {
    for numbered in [false, true] {
        let mut dst = Dst::new();
        let request = u32::from(numbered);
        if numbered {
            assert_eq!(
                messages(&dst.resized(px(800, 600), 0)),
                vec![state_resize(request, px(800, 600), false)]
            );
        }
        assert_eq!(
            dst.geometry(open_size(), request, Some(true), 300),
            vec![Output::ProxyFullscreen {
                key: key(),
                fullscreen: true
            }]
        );
        // No Fullscreen(false) or Resized callback: the host coalesces the two flag commands.
        assert_eq!(
            dst.geometry(px(700, 500), request, Some(false), 301),
            vec![
                Output::ProxyFullscreen {
                    key: key(),
                    fullscreen: false
                },
                Output::ProxyGeometry {
                    key: key(),
                    size: px(700, 500),
                    parking: ParkingKind::Twin
                },
            ]
        );
        assert_eq!(
            dst.geometry(px(900, 700), request, Some(false), 302),
            vec![Output::ProxyGeometry {
                key: key(),
                size: px(900, 700),
                parking: ParkingKind::Twin
            }]
        );
        if numbered {
            assert!(dst.geometry(open_size(), 0, Some(true), 303).is_empty());
        }
    }
}

#[test]
fn cached_stand_in_is_selected_at_first_park_in_both_event_orders() {
    for kind in [PlatformParking::Twin, PlatformParking::Mirror] {
        for candidate_first in [false, true] {
            let mut src = Src::offered();
            src.toggle(WindowState::Normal, 0);
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
            let candidate = Input::Windows(WindowEvent::Added(candidate(11, DISPLAY)));
            if candidate_first {
                src.at(candidate.clone(), 10);
            }
            src.toggle(WindowState::Hidden, 11);
            if !candidate_first {
                src.at(candidate, 12);
            }
            let out = src.result(open_size(), false, kind, 20);
            let target = if kind == PlatformParking::Twin {
                CaptureTarget::Display(DISPLAY)
            } else {
                CaptureTarget::Window(WindowId(11))
            };
            assert!(
                out.iter()
                    .any(|o| matches!(o, Output::StartCapture { target: t, .. } if *t == target)),
                "{out:?}"
            );
            let out = started(&mut src, StreamId(2), 21);
            projection_survives(&out);
            assert_eq!(out, vec![state_repark(open_size(), true)]);
            let out = src.result(whole(), true, kind, 22);
            assert!(
                messages(&out).iter().any(|m| matches!(
                    m,
                    Message::Geometry {
                        fullscreen: Some(true),
                        ..
                    }
                )),
                "{out:?}"
            );
            assert!(src.at(Input::Command(Command::Return(key())), 23).contains(
                &Output::StopCapture {
                    stream: StreamId(2)
                }
            ));
        }
    }
}

#[test]
fn obsolete_initial_start_failure_reconciles_before_replacement() {
    let mut src = Src::offered();
    src.toggle(WindowState::Normal, 0);
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
    src.result(open_size(), false, PlatformParking::Mirror, 0);
    src.at(
        Input::Windows(WindowEvent::Added(candidate(11, DISPLAY))),
        1,
    );
    src.toggle(WindowState::Hidden, 2);
    assert_eq!(
        failed_start(&mut src, 3),
        vec![state_repark(open_size(), true)]
    );
    let out = src.result(whole(), true, PlatformParking::Mirror, 4);
    assert!(
        out.iter().any(|o| matches!(
            o,
            Output::StartCapture {
                target: CaptureTarget::Window(WindowId(11)),
                ..
            }
        )),
        "{out:?}"
    );
    assert!(started(&mut src, StreamId(2), 5).is_empty());
}

#[test]
fn cached_first_park_candidates_still_require_pid_display_and_grace() {
    for (pid, display, parked_at) in [
        (Some(7), DISPLAY, 20),
        (Some(42), DisplayId(8), 20),
        (Some(42), DISPLAY, 1_012),
    ] {
        let mut src = Src::offered();
        src.toggle(WindowState::Normal, 0);
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
        let mut info = candidate(11, display);
        info.pid = pid;
        src.at(Input::Windows(WindowEvent::Added(info)), 10);
        src.toggle(WindowState::Hidden, 11);
        let out = src.result(open_size(), false, PlatformParking::Mirror, parked_at);
        projection_survives(&out);
        assert!(
            !out.iter().any(|o| matches!(o, Output::StartCapture { .. })),
            "{out:?}"
        );
        projection_survives(&src.tick(30_000));
    }
}
