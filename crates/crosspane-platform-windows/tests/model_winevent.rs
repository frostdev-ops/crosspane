use std::time::Duration;

use crosspane_platform::{WindowEvent, WindowRole, WindowState};
use crosspane_platform_windows::model::winevent::*;
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelRect, PixelSize, PointLogical, RectLogical, SizeLogical, SizeMm,
    euclid::point2,
};
use crosspane_types::id::{DisplayId, WindowId};
use crosspane_types::time::MonoTime;
use proptest::prelude::*;

fn ms(value: u64) -> MonoTime {
    MonoTime::from_nanos(value * 1_000_000)
}

fn rect(x: i32, y: i32, width: i32, height: i32) -> PixelRect {
    PixelRect::new(point2(x, y), point2(x + width, y + height))
}

fn displays() -> Vec<DisplayInfo> {
    vec![DisplayInfo {
        id: DisplayId(7),
        name: "fake monitor".into(),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(300.0, 200.0),
            pixel_size: PixelSize::new(1920, 1080),
            scale: 1.5,
            logical_origin: PointLogical::new(-1280.0, 200.0),
        },
        refresh_millihz: 60_000,
        color_space: ColorSpace::Srgb,
        hdr: false,
    }]
}

fn popup(hwnd: u64, next: u64, visible: bool) -> PopupProbe {
    PopupProbe {
        hwnd,
        last_active_popup: next,
        visible,
        ex_style: 0,
    }
}

fn probe(hwnd: u64) -> WindowProbe {
    WindowProbe {
        title: "injected title".into(),
        app_id: "injected application".into(),
        pid: 123,
        style: 0,
        ex_style: 0,
        owner: None,
        root_owner: hwnd,
        popup_chain: vec![popup(hwnd, hwnd, true)],
        visible: true,
        iconic: false,
        cloaked: 0,
        rect_physical: rect(-1800, 60, 900, 600),
        frame_physical: None,
        monitor: Some(DisplayId(7)),
        monitor_physical: Some(rect(-1920, 0, 1920, 1080)),
    }
}

fn event(event: u32, hwnd: u64, at: u64) -> RawWinEvent {
    RawWinEvent {
        event,
        hwnd,
        id_object: OBJID_WINDOW,
        id_child: CHILDID_SELF,
        at: ms(at),
    }
}

fn table() -> WindowTable {
    WindowTable::new(42, Duration::from_millis(50))
}

fn request(table: &mut WindowTable, kind: u32, hwnd: u64) -> ProbeRequest {
    match table.on_event(event(kind, hwnd, 0)).as_slice() {
        [Action::Probe(request)] => *request,
        other => panic!("expected exactly one probe, got {other:?}"),
    }
}

fn seed(table: &mut WindowTable, hwnd: u64) {
    assert!(matches!(
        table.seed(vec![(hwnd, probe(hwnd))], &displays()).as_slice(),
        [WindowEvent::Added(window)] if window.id == WindowId(hwnd)
    ));
}

macro_rules! probe_event {
    ($name:ident, $kind:ident) => {
        #[test]
        fn $name() {
            let mut t = table();
            let request = request(&mut t, $kind, 10);
            assert_eq!(request.hwnd, 10);
            assert_eq!(t.windows().count(), 0);
            assert!(matches!(
                t.on_probe(request, Some(probe(10)), &displays(), ms(1)).as_slice(),
                [WindowEvent::Added(window)] if window.id == WindowId(10)
            ));
        }
    };
}

probe_event!(create_requests_a_probe, EVENT_OBJECT_CREATE);
probe_event!(show_requests_a_probe, EVENT_OBJECT_SHOW);
probe_event!(uncloaked_requests_a_probe, EVENT_OBJECT_UNCLOAKED);
probe_event!(minimize_end_requests_a_probe, EVENT_SYSTEM_MINIMIZEEND);
probe_event!(name_change_requests_a_probe, EVENT_OBJECT_NAMECHANGE);
probe_event!(move_size_end_requests_a_probe, EVENT_SYSTEM_MOVESIZEEND);
probe_event!(reorder_requests_a_probe, EVENT_OBJECT_REORDER);

#[test]
fn hide_requests_a_probe_and_retains_hidden_until_destroy() {
    for iconic in [false, true] {
        let mut t = table();
        seed(&mut t, 10);
        let request = request(&mut t, EVENT_OBJECT_HIDE, 10);
        let mut hidden = probe(10);
        hidden.visible = false;
        hidden.iconic = iconic;
        assert!(matches!(
            t.on_probe(request, Some(hidden), &displays(), ms(2)).as_slice(),
            [WindowEvent::Changed(window)] if window.state == WindowState::Hidden
        ));
        assert_eq!(t.windows().count(), 1);
        assert_eq!(
            t.on_event(event(EVENT_OBJECT_DESTROY, 10, 3)),
            vec![Action::Emit(WindowEvent::Removed(WindowId(10)))]
        );
    }
}

#[test]
fn cloaked_requests_a_probe_and_retains_each_cloak_reason_as_hidden() {
    for cloak in [
        DWM_CLOAKED_APP,
        DWM_CLOAKED_SHELL,
        DWM_CLOAKED_INHERITED,
        7,
        8,
    ] {
        let mut t = table();
        seed(&mut t, 10);
        let request = request(&mut t, EVENT_OBJECT_CLOAKED, 10);
        let mut hidden = probe(10);
        hidden.cloaked = cloak;
        hidden.iconic = true;
        assert!(matches!(
            t.on_probe(request, Some(hidden.clone()), &displays(), ms(2)).as_slice(),
            [WindowEvent::Changed(window)] if window.state == WindowState::Hidden
        ));
        assert_eq!(t.windows().count(), 1);
        assert!(table().seed(vec![(10, hidden)], &displays()).is_empty());
    }
}

#[test]
fn minimize_start_changes_only_a_tabled_window_once_and_retires_prior_probe() {
    let mut t = table();
    seed(&mut t, 10);
    let old = request(&mut t, EVENT_OBJECT_SHOW, 10);
    assert!(matches!(
        t.on_event(event(EVENT_SYSTEM_MINIMIZESTART, 10, 1)).as_slice(),
        [Action::Emit(WindowEvent::Changed(window))] if window.state == WindowState::Minimized
    ));
    assert!(
        t.on_event(event(EVENT_SYSTEM_MINIMIZESTART, 10, 2))
            .is_empty()
    );
    assert!(
        t.on_event(event(EVENT_SYSTEM_MINIMIZESTART, 99, 2))
            .is_empty()
    );
    assert!(
        t.on_probe(old, Some(probe(10)), &displays(), ms(3))
            .is_empty()
    );
    assert_eq!(t.windows().next().unwrap().state, WindowState::Minimized);
}

#[test]
fn create_then_minimize_start_preserves_admission_and_minimized_state() {
    for iconic in [false, true] {
        let mut t = table();
        let pending = request(&mut t, EVENT_OBJECT_CREATE, 10);
        assert!(
            t.on_event(event(EVENT_SYSTEM_MINIMIZESTART, 10, 1))
                .is_empty()
        );
        let mut answer = probe(10);
        answer.iconic = iconic;
        assert!(matches!(
            t.on_probe(pending, Some(answer), &displays(), ms(2)).as_slice(),
            [WindowEvent::Added(w)] if w.id == WindowId(10) && w.state == WindowState::Minimized
        ));
        assert_eq!(t.windows().next().unwrap().state, WindowState::Minimized);
        assert!(
            t.on_probe(pending, Some(probe(10)), &displays(), ms(3))
                .is_empty()
        );
        assert_eq!(t.next_deadline(), None);
    }
}

#[test]
fn foreground_reports_tabled_or_none_including_null_unlisted_window() {
    let mut t = table();
    seed(&mut t, 10);
    for (hwnd, focused) in [(10, Some(WindowId(10))), (99, None), (0, None)] {
        assert_eq!(
            t.on_event(event(EVENT_SYSTEM_FOREGROUND, hwnd, 1)),
            vec![Action::Emit(WindowEvent::Focused(focused))]
        );
    }
    assert_eq!(t.on_foreground(None), vec![WindowEvent::Focused(None)]);
    assert_eq!(t.on_foreground(Some(99)), vec![WindowEvent::Focused(None)]);
}

#[test]
fn location_changes_coalesce_ten_events_at_first_deadline_without_sliding() {
    let mut t = table();
    for at in 1..=10 {
        assert!(
            t.on_event(event(EVENT_OBJECT_LOCATIONCHANGE, 10, at))
                .is_empty()
        );
    }
    assert_eq!(t.next_deadline(), Some(ms(51)));
    assert!(t.poll(ms(50)).is_empty());
    assert!(matches!(t.poll(ms(51)).as_slice(), [Action::Probe(r)] if r.hwnd == 10));
    assert_eq!(t.next_deadline(), None);
    assert!(t.poll(ms(100)).is_empty());
}

#[test]
fn object_and_child_filters_reject_every_event_including_cursor_and_unknown_events() {
    let kinds = [
        EVENT_OBJECT_CREATE,
        EVENT_OBJECT_DESTROY,
        EVENT_OBJECT_SHOW,
        EVENT_OBJECT_HIDE,
        EVENT_OBJECT_REORDER,
        EVENT_OBJECT_LOCATIONCHANGE,
        EVENT_OBJECT_NAMECHANGE,
        EVENT_OBJECT_CLOAKED,
        EVENT_OBJECT_UNCLOAKED,
        EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_MINIMIZESTART,
        EVENT_SYSTEM_MINIMIZEEND,
        EVENT_SYSTEM_MOVESIZEEND,
    ];
    let mut t = table();
    seed(&mut t, 10);
    for kind in kinds {
        for (id_object, id_child) in [(-9, 0), (1, 0), (0, 1), (0, -1)] {
            let mut raw = event(kind, 10, 0);
            raw.id_object = id_object;
            raw.id_child = id_child;
            assert!(t.on_event(raw).is_empty());
            assert_eq!(t.windows().count(), 1);
            assert_eq!(t.next_deadline(), None);
        }
    }
    assert!(t.on_event(event(0xdead, 10, 0)).is_empty());
    assert!(t.on_event(event(EVENT_OBJECT_CREATE, 0, 0)).is_empty());
}

#[test]
fn unknown_destroy_is_silent_and_none_after_create_does_not_add_or_invent_closure() {
    let mut t = table();
    assert!(t.on_event(event(EVENT_OBJECT_DESTROY, 99, 0)).is_empty());
    let new = request(&mut t, EVENT_OBJECT_CREATE, 10);
    assert!(t.on_probe(new, None, &displays(), ms(1)).is_empty());
    assert_eq!(t.windows().count(), 0);
    seed(&mut t, 10);
    let existing = request(&mut t, EVENT_OBJECT_SHOW, 10);
    assert!(t.on_probe(existing, None, &displays(), ms(2)).is_empty());
    assert_eq!(t.windows().count(), 1);
}

#[test]
fn unchanged_probe_is_silent_and_changed_frozen_info_is_emitted_once() {
    let mut t = table();
    seed(&mut t, 10);
    let same = request(&mut t, EVENT_OBJECT_NAMECHANGE, 10);
    assert!(
        t.on_probe(same, Some(probe(10)), &displays(), ms(1))
            .is_empty()
    );
    let changed = request(&mut t, EVENT_OBJECT_NAMECHANGE, 10);
    let mut p = probe(10);
    p.title = "changed injected title".into();
    let events = t.on_probe(changed, Some(p), &displays(), ms(2));
    assert!(
        matches!(events.as_slice(), [WindowEvent::Changed(w)] if w.title == "changed injected title")
    );
    assert!(
        t.on_probe(changed, Some(probe(10)), &displays(), ms(3))
            .is_empty()
    );
    assert_eq!(t.windows().next().unwrap().title, "changed injected title");
}

#[test]
fn unavailable_popup_classification_retains_cached_window_but_confirmed_exclusion_removes() {
    for chain in [
        Vec::new(),
        vec![popup(10, 20, true)],
        vec![popup(10, 10, true); 2],
        vec![popup(10, 20, false), popup(20, 10, false)],
        (10..75).map(|hwnd| popup(hwnd, hwnd, true)).collect(),
    ] {
        let mut t = table();
        seed(&mut t, 10);
        let cached = t.windows().next().unwrap().clone();
        let pending = request(&mut t, EVENT_OBJECT_NAMECHANGE, 10);
        let mut incomplete = probe(10);
        incomplete.title = "unavailable classification must not update cached facts".into();
        incomplete.popup_chain = chain;
        assert!(
            t.on_probe(pending, Some(incomplete), &displays(), ms(1))
                .is_empty()
        );
        assert_eq!(t.windows().next(), Some(&cached));
        assert_eq!(t.windows().count(), 1);

        let pending = request(&mut t, EVENT_OBJECT_REORDER, 10);
        let mut excluded = probe(10);
        excluded.root_owner = 20;
        excluded.popup_chain = vec![popup(20, 20, true)];
        assert_eq!(
            t.on_probe(pending, Some(excluded), &displays(), ms(2)),
            vec![WindowEvent::Removed(WindowId(10))]
        );
        assert_eq!(t.windows().count(), 0);
    }
}

#[test]
fn hwnd_reuse_emits_removed_then_added_and_cannot_accept_old_or_superseded_answers() {
    let mut t = table();
    let original = request(&mut t, EVENT_OBJECT_CREATE, 10);
    assert_eq!(
        t.on_probe(original, Some(probe(10)), &displays(), ms(1))
            .len(),
        1
    );
    let late = request(&mut t, EVENT_OBJECT_SHOW, 10);
    t.on_event(event(EVENT_OBJECT_LOCATIONCHANGE, 10, 2));
    assert_eq!(
        t.on_event(event(EVENT_OBJECT_DESTROY, 10, 3)),
        vec![Action::Emit(WindowEvent::Removed(WindowId(10)))]
    );
    assert_eq!(t.next_deadline(), None);
    assert!(
        t.on_probe(late, Some(probe(10)), &displays(), ms(4))
            .is_empty()
    );
    let reused = request(&mut t, EVENT_OBJECT_CREATE, 10);
    assert!(reused.sequence > late.sequence);
    assert!(
        t.on_probe(late, Some(probe(10)), &displays(), ms(5))
            .is_empty()
    );
    let mut replacement = probe(10);
    replacement.pid = 456;
    assert!(matches!(
        t.on_probe(reused, Some(replacement), &displays(), ms(6)).as_slice(),
        [WindowEvent::Added(w)] if w.id == WindowId(10) && w.pid == Some(456)
    ));
    let stale = request(&mut t, EVENT_OBJECT_SHOW, 10);
    let latest = request(&mut t, EVENT_OBJECT_SHOW, 10);
    assert!(latest.sequence > stale.sequence);
    assert!(
        t.on_probe(stale, Some(probe(10)), &displays(), ms(7))
            .is_empty()
    );
    assert_eq!(t.windows().next().unwrap().pid, Some(456));
}

#[test]
fn classifier_rejects_tool_child_cloaked_own_process_and_accepts_app_override() {
    let p = probe(10);
    assert_eq!(classify(10, &p, 42), Some(WindowRole::Toplevel));
    for change in 0..7 {
        let mut changed = p.clone();
        match change {
            0 => changed.ex_style = WS_EX_TOOLWINDOW,
            1 => changed.ex_style = WS_EX_TOOLWINDOW | WS_EX_APPWINDOW,
            2 => changed.style = WS_CHILD,
            3 => changed.pid = 42,
            4 => changed.pid = 0,
            5 => changed.cloaked = DWM_CLOAKED_APP,
            _ => changed.visible = false,
        }
        assert_eq!(classify(10, &changed, 42), None);
    }
    assert_eq!(classify(0, &p, 42), None);
    let mut owned = p;
    owned.owner = Some(20);
    owned.root_owner = 20;
    owned.popup_chain = vec![popup(20, 10, true), popup(10, 10, true)];
    assert_eq!(classify(10, &owned, 42), None);
    owned.ex_style = WS_EX_APPWINDOW;
    assert_eq!(classify(10, &owned, 42), Some(WindowRole::Dialog));
}

#[test]
fn historical_popup_walk_requires_complete_unique_acyclic_bounded_observations() {
    let mut p = probe(10);
    p.popup_chain = vec![popup(10, 20, true), popup(20, 20, true)];
    assert_eq!(classify(10, &p, 42), Some(WindowRole::Toplevel));
    p.popup_chain[1].visible = false;
    assert_eq!(classify(10, &p, 42), None, "the walk moved from the root");
    p.popup_chain[1].last_active_popup = 10;
    p.popup_chain[0].visible = false;
    assert_eq!(classify(10, &p, 42), None, "traversed cycle");
    p.popup_chain = vec![popup(10, 20, true)];
    assert_eq!(classify(10, &p, 42), None, "missing referenced popup");
    p.popup_chain = vec![popup(10, 10, true); 2];
    assert_eq!(classify(10, &p, 42), None, "duplicate row");
    p.popup_chain = (10..75).map(|hwnd| popup(hwnd, hwnd, true)).collect();
    for style in [0, WS_EX_APPWINDOW] {
        p.ex_style = style;
        assert_eq!(classify(10, &p, 42), None, "too many supplied rows");
    }
}

#[test]
fn state_matrix_prioritizes_hidden_then_iconic_and_uses_actual_frame_for_fullscreen() {
    let monitor = rect(-1920, 0, 1920, 1080);
    for iconic in [false, true] {
        for visible in [false, true] {
            for cloak in [0, 1, 2, 4, 7, 8] {
                for full in [false, true] {
                    let mut p = probe(10);
                    p.iconic = iconic;
                    p.visible = visible;
                    p.cloaked = cloak;
                    p.frame_physical = full.then_some(monitor);
                    let expected = if !visible || cloak != 0 {
                        WindowState::Hidden
                    } else if iconic {
                        WindowState::Minimized
                    } else if full {
                        WindowState::Fullscreen
                    } else {
                        WindowState::Normal
                    };
                    assert_eq!(window_state(&p, Some(monitor)), expected);
                }
            }
        }
    }
    let mut p = probe(10);
    p.rect_physical = monitor;
    assert_eq!(window_state(&p, Some(monitor)), WindowState::Fullscreen);
    assert_eq!(window_state(&p, None), WindowState::Normal);
    p.frame_physical = Some(rect(-1919, 0, 1920, 1080));
    assert_eq!(window_state(&p, Some(monitor)), WindowState::Normal);
}

#[test]
fn physical_monitor_origin_is_subtracted_before_frozen_fractional_scale_transform() {
    let mut t = table();
    seed(&mut t, 10);
    let w = t.windows().next().unwrap();
    assert_eq!(
        w.frame,
        RectLogical::new(
            PointLogical::new(-1200.0, 240.0),
            SizeLogical::new(600.0, 400.0)
        )
    );
    assert_eq!(w.display, Some(DisplayId(7)));
    assert_eq!(w.parent, None);
    assert_eq!(w.pid, Some(123));
}

#[test]
fn invalid_unknown_or_duplicate_monitor_geometry_is_not_invented() {
    for failure in 0..7 {
        let mut t = table();
        let mut p = probe(10);
        let mut ds = displays();
        match failure {
            0 => p.monitor = None,
            1 => p.monitor_physical = None,
            2 => ds[0].geometry.scale = 0.0,
            3 => ds.push(ds[0].clone()),
            4 => p.rect_physical = rect(0, 0, 0, 100),
            5 => p.monitor_physical = Some(rect(0, 0, -100, 100)),
            _ => p.monitor_physical = Some(rect(-1920, 0, 1921, 1080)),
        }
        assert!(t.seed(vec![(10, p)], &ds).is_empty());
        assert_eq!(t.windows().count(), 0);
    }
}

#[test]
fn windows_numeric_constants_match_documented_values() {
    assert_eq!(
        [
            EVENT_OBJECT_CREATE,
            EVENT_OBJECT_DESTROY,
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_HIDE,
            EVENT_OBJECT_REORDER,
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_NAMECHANGE,
            EVENT_OBJECT_CLOAKED,
            EVENT_OBJECT_UNCLOAKED
        ],
        [
            0x8000, 0x8001, 0x8002, 0x8003, 0x8004, 0x800b, 0x800c, 0x8017, 0x8018
        ]
    );
    assert_eq!(
        [
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_MINIMIZESTART,
            EVENT_SYSTEM_MINIMIZEEND,
            EVENT_SYSTEM_MOVESIZEEND
        ],
        [3, 0x16, 0x17, 0xb]
    );
    assert_eq!((OBJID_WINDOW, CHILDID_SELF), (0, 0));
    assert_eq!(
        (WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_APPWINDOW),
        (0x40000000, 0x80, 0x40000)
    );
    assert_eq!(
        (
            DWMWA_CLOAKED,
            DWM_CLOAKED_APP,
            DWM_CLOAKED_SHELL,
            DWM_CLOAKED_INHERITED
        ),
        (14, 1, 2, 4)
    );
    assert_eq!((WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS), (0, 2));
}

proptest! {
    #[test]
    fn table_contains_only_currently_answered_probed_windows(
        operations in prop::collection::vec((0_u8..6, 1_u64..12), 0..200)
    ) {
        let mut t = table();
        let mut admitted = std::collections::BTreeSet::new();
        for (operation, hwnd) in operations {
            match operation {
                0 => {
                    let r = request(&mut t, EVENT_OBJECT_CREATE, hwnd);
                    t.on_probe(r, Some(probe(hwnd)), &displays(), ms(0));
                    admitted.insert(hwnd);
                }
                1 => {
                    t.on_event(event(EVENT_OBJECT_DESTROY, hwnd, 0));
                    admitted.remove(&hwnd);
                }
                2 => { t.on_event(event(EVENT_OBJECT_LOCATIONCHANGE, hwnd, 0)); }
                3 => { t.on_event(event(EVENT_SYSTEM_MINIMIZESTART, hwnd, 0)); }
                4 => {
                    let r = request(&mut t, EVENT_OBJECT_SHOW, hwnd);
                    t.on_probe(r, None, &displays(), ms(0));
                }
                _ => { t.on_foreground(Some(hwnd)); }
            }
            let actual: std::collections::BTreeSet<_> = t.windows().map(|w| w.id.0).collect();
            prop_assert_eq!(&actual, &admitted);
        }
    }
}
