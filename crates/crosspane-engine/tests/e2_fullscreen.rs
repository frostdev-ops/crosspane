//! WP-2.45b: a source window's state change (Normal to Fullscreen or Hidden and back) is a
//! re-park trigger, a frame move while the window is not `Normal` is not, and windows that are
//! `Hidden` are not offered for browsing. A change that can't be followed at once (a park in
//! flight, the `REPARK_GAP`) is deferred, never dropped: it is looked at again when the park
//! finishes and when the gap ends, with no further window event needed. The source role alone,
//! driven by window events and platform results; all time is explicit and nothing sleeps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_engine::e2::E2;
use crosspane_engine::{Command, EngineConfig, Input, Output, ProjectionKey};
use crosspane_input::journal::MemoryJournal;
use crosspane_platform::{
    LockState, Parked, ParkingKind as PlatformParking, SessionEvent, SessionState, StreamId,
    WindowEvent, WindowInfo, WindowRole, WindowState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ControlMessage};
use crosspane_protocol::projection::{
    ParkingKind, ProjectionEndReason as Reason, ProjectionMessage as Message,
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
        projection: ID,
        size,
        parking: ParkingKind::Twin,
        answers,
    }
}
fn resize_parked(size: PixelSize, scale: f64) -> Output {
    Output::ResizeParked {
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
                result: Ok(parked(open_size())),
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
        self.parked(size, now + 10)
    }
}

#[test]
fn normal_to_fullscreen_is_one_re_park_and_the_geometry_follows_the_parked() {
    let mut src = Src::live();
    let out = src.changed(window_in(WindowState::Fullscreen, whole_frame()), 3_000);
    assert_eq!(out, vec![resize_parked(open_size(), SCALE)]);
    // While that runs, a further change waits for its result, whatever it is.
    let again = window_in(
        WindowState::Fullscreen,
        RectLogical::new(PointLogical::new(0.0, 30.0), SizeLogical::new(855.0, 673.0)),
    );
    assert!(src.changed(again, 3_005).is_empty());
    // The platform answers with the whole display: the capture crop follows it, then the
    // geometry the destination grows its proxy to.
    let out = src.parked(whole(), 3_010);
    assert_eq!(
        out,
        vec![
            Output::SetCaptureCrop {
                stream: STREAM,
                crop: Some(parked(whole()).content),
            },
            sent(DST, geometry(whole(), 0)),
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
        assert_eq!(out, vec![resize_parked(open_size(), SCALE)], "{state:?}");
        src.parked(open_size(), 3_010);
        let out = src.changed(window(), 5_100);
        assert_eq!(out, vec![resize_parked(open_size(), SCALE)], "{state:?}");
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
    assert_eq!(out, vec![resize_parked(a, SCALE)]);
    assert!(src.request(2, px(900, 700), 3_001).is_empty());
    let out = src.parked(whole(), 3_010);
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
        vec![(5_000, resize_parked(open_size(), SCALE))]
    );
    // ...whose result is the whole display, and that is the end of it.
    assert_eq!(src.parked(whole(), 5_010), finished(whole()));
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
    assert_eq!(out, vec![resize_parked(open_size(), SCALE)]);
    // Out and back while it runs: the window is what the park was based on when it ends.
    assert!(src.changed(window(), 3_100).is_empty());
    assert!(
        src.changed(window_in(WindowState::Fullscreen, whole_frame()), 3_200)
            .is_empty()
    );
    assert_eq!(src.parked(whole(), 3_300), finished(whole()));
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
    assert_eq!(src.parked(whole(), 30), finished(whole()));
    assert_eq!(src.deadline(), None);
}
