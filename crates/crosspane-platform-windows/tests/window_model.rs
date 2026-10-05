#![allow(clippy::unwrap_used)]
use crosspane_platform::{WindowEvent, WindowRole, WindowState};
use crosspane_platform_windows::model::{
    window::{Identity, Observation, Windows},
    winevent::*,
};
use crosspane_types::{
    geom::{PointLogical, RectLogical, SizeLogical},
    id::DisplayId,
    time::MonoTime,
};

fn normal(hwnd: u64) -> Observation {
    Observation {
        identity: Identity {
            hwnd,
            pid: 41,
            tid: 42,
            process_created: 43,
        },
        title: "owned fixture".into(),
        app_id: "fixture.exe".into(),
        class: "fixture".into(),
        style: 0x00c0_0000,
        ex_style: 0,
        owner: None,
        root: true,
        visible: true,
        iconic: false,
        cloaked: false,
        current_desktop: Some(true),
        display: DisplayId(1),
        frame: RectLogical::new(
            PointLogical::new(10.0, 20.0),
            SizeLogical::new(300.0, 200.0),
        ),
        fills_monitor: false,
    }
}

#[test]
fn normal_dialog_tool_and_child_popup_admission() {
    let mut table = Windows::default();
    assert!(
        matches!(table.observe(normal(1)).as_slice(), [WindowEvent::Added(w)] if w.role == WindowRole::Toplevel)
    );
    let mut dialog = normal(2);
    dialog.owner = Some(1);
    dialog.ex_style = WS_EX_TOOLWINDOW;
    assert!(
        matches!(table.observe(dialog).as_slice(), [WindowEvent::Added(w)] if w.role == WindowRole::Dialog)
    );
    let mut tool = normal(3);
    tool.ex_style = WS_EX_TOOLWINDOW;
    assert!(table.observe(tool).is_empty());
    let mut popup = normal(4);
    popup.class = "#32768".into();
    popup.owner = Some(1);
    assert!(
        matches!(table.observe(popup.clone()).as_slice(), [WindowEvent::Added(w)] if w.role == WindowRole::Popup && w.parent == Some(table.list()[0].id))
    );
    popup.identity.hwnd = 5;
    popup.owner = Some(99);
    assert!(table.observe(popup).is_empty());
}

#[test]
fn startup_unknown_hidden_and_titleless_are_not_admitted() {
    for variant in 0..4 {
        let mut w = normal(1);
        match variant {
            0 => w.current_desktop = None,
            1 => w.cloaked = true,
            2 => w.visible = false,
            _ => w.title.clear(),
        }
        assert!(Windows::default().observe(w).is_empty());
    }
}

#[test]
fn admitted_hidden_minimized_and_fullscreen_remain_until_close() {
    let mut table = Windows::default();
    let mut w = normal(1);
    table.observe(w.clone());
    let id = table.list()[0].id;
    w.cloaked = true;
    assert!(
        matches!(table.observe(w.clone()).as_slice(), [WindowEvent::Changed(w)] if w.state == WindowState::Hidden && w.id == id)
    );
    w.cloaked = false;
    w.iconic = true;
    assert!(
        matches!(table.observe(w.clone()).as_slice(), [WindowEvent::Changed(w)] if w.state == WindowState::Minimized)
    );
    w.iconic = false;
    w.fills_monitor = true;
    w.style = 0;
    assert!(
        matches!(table.observe(w).as_slice(), [WindowEvent::Changed(w)] if w.state == WindowState::Fullscreen)
    );
    assert!(matches!(table.close(1, None).as_slice(), [WindowEvent::Removed(gone)] if *gone == id));
}

#[test]
fn reused_hwnd_changes_id_and_stale_close_cannot_remove_replacement() {
    let mut table = Windows::default();
    let old = normal(1);
    table.observe(old.clone());
    let first = table.list()[0].id;
    let mut next = old.clone();
    next.identity.process_created += 1;
    let changes = table.observe(next);
    assert!(
        matches!(changes.as_slice(), [WindowEvent::Removed(id), WindowEvent::Added(w)] if *id == first && w.id != first)
    );
    assert!(table.close(1, Some(old.identity)).is_empty());
    assert_eq!(table.list().len(), 1);
}

#[test]
fn create_changes_generation_even_for_same_native_identity() {
    let mut table = Windows::default();
    table.observe(normal(1));
    let first = table.list()[0].id;
    table.event(RawWinEvent {
        event: EVENT_OBJECT_CREATE,
        hwnd: 1,
        id_object: OBJID_WINDOW,
        id_child: CHILDID_SELF,
        at: MonoTime::ZERO,
    });
    table.observe(normal(1));
    assert_ne!(first, table.list()[0].id);
}

#[test]
fn locations_coalesce_without_sliding_and_nonwindow_events_are_ignored() {
    let mut table = Windows::default();
    for millis in [0, 25, 49] {
        assert!(
            !table
                .event(RawWinEvent {
                    event: EVENT_OBJECT_LOCATIONCHANGE,
                    hwnd: 1,
                    id_object: 0,
                    id_child: 0,
                    at: MonoTime::from_nanos(millis * 1_000_000)
                })
                .1
        );
    }
    assert!(table.due(MonoTime::from_nanos(49_000_000)).is_empty());
    assert_eq!(table.due(MonoTime::from_nanos(50_000_000)), vec![1]);
    assert!(
        !table
            .event(RawWinEvent {
                event: EVENT_OBJECT_SHOW,
                hwnd: 1,
                id_object: -4,
                id_child: 0,
                at: MonoTime::ZERO
            })
            .1
    );
}

#[test]
fn focused_identity_tracks_only_admitted_windows() {
    let mut table = Windows::default();
    table.observe(normal(1));
    let id = table.list()[0].id;
    assert!(matches!(table.focus(Some(1)).as_slice(), [WindowEvent::Focused(Some(w))] if *w == id));
    assert_eq!(table.identity(id), Some(normal(1).identity));
    assert!(matches!(
        table.focus(Some(99)).as_slice(),
        [WindowEvent::Focused(None)]
    ));
}

#[test]
fn activation_refuses_hidden_unknown_desktop_and_changed_identity() {
    use crosspane_platform_windows::model::window::may_activate;
    let expected = normal(1).identity;
    assert!(may_activate(
        expected,
        expected,
        Some(true),
        true,
        WindowState::Normal
    ));
    assert!(may_activate(
        expected,
        expected,
        Some(true),
        true,
        WindowState::Minimized
    ));
    for state in [
        WindowState::Normal,
        WindowState::Minimized,
        WindowState::Hidden,
    ] {
        assert!(!may_activate(expected, expected, None, true, state));
        assert!(!may_activate(expected, expected, Some(false), true, state));
        assert!(!may_activate(expected, expected, Some(true), false, state));
    }
    assert!(!may_activate(
        expected,
        expected,
        Some(true),
        true,
        WindowState::Hidden
    ));
    for field in 0..3 {
        let mut changed = expected;
        match field {
            0 => changed.pid += 1,
            1 => changed.tid += 1,
            _ => changed.process_created += 1,
        }
        assert!(!may_activate(
            expected,
            changed,
            Some(true),
            true,
            WindowState::Normal
        ));
    }
}

#[test]
fn invisible_iconic_window_stays_hidden_and_is_never_restorable_by_activation() {
    use crosspane_platform_windows::model::window::may_activate;
    let mut table = Windows::default();
    let mut observation = normal(1);
    table.observe(observation.clone());
    observation.visible = false;
    observation.iconic = true;
    assert_eq!(observation.state(), WindowState::Hidden);
    assert!(!may_activate(
        observation.identity,
        observation.identity,
        Some(true),
        true,
        observation.state()
    ));
    assert!(
        matches!(table.observe(observation).as_slice(), [WindowEvent::Changed(w)] if w.state == WindowState::Hidden)
    );
}

#[test]
fn monitor_conversion_uses_retained_ids_negative_origins_and_fractional_scale() {
    use crosspane_platform_windows::model::{
        geometry::{DisplayIds, MonitorProbe},
        window::logical_frame,
    };
    let probes = vec![MonitorProbe {
        device_path: "fixture-monitor".into(),
        name: "fixture-display".into(),
        rc_monitor: [-1920, -1080, 0, 0],
        rc_work: [-1920, -1080, 0, 0],
        primary: true,
        dpi: 144,
        refresh_millihz: 60000,
        edid: None,
        twin: false,
        quarter_turns: 0,
    }];
    let mut ids = DisplayIds::default();
    let (id, frame) = logical_frame(
        [-1770, -930, -1470, -630],
        probes[0].rc_monitor,
        &probes[0].name,
        &probes,
        &mut ids,
    )
    .unwrap();
    assert_eq!(frame.origin, PointLogical::new(100.0, 100.0));
    assert_eq!(frame.size, SizeLogical::new(200.0, 200.0));
    assert_eq!(id, ids.assign("fixture-monitor").unwrap());
    assert_eq!(
        logical_frame(
            [-1770, -930, -1470, -630],
            probes[0].rc_monitor,
            &probes[0].name,
            &probes,
            &mut ids
        )
        .unwrap()
        .0,
        id
    );
    assert!(
        logical_frame(
            [-1770, -930, -1470, -630],
            probes[0].rc_monitor,
            "stale",
            &probes,
            &mut ids
        )
        .is_err()
    );
    assert!(
        logical_frame(
            [-1770, -930, -1470, -630],
            probes[0].rc_monitor,
            &probes[0].name,
            &[probes[0].clone(), probes[0].clone()],
            &mut ids
        )
        .is_err()
    );
}

proptest::proptest! {
    #[test]
    fn every_observed_lifetime_gets_a_distinct_id(handles in proptest::collection::vec(1_u64..32, 1..200)) {
        let mut table = Windows::default(); let mut allocated = std::collections::BTreeSet::new();
        for handle in handles {
            table.close(handle, None);
            table.observe(normal(handle));
            let id = table.list().into_iter().find(|w| table.identity(w.id).unwrap().hwnd == handle).unwrap().id;
            proptest::prop_assert!(allocated.insert(id));
            proptest::prop_assert!(table.observe(normal(handle)).is_empty());
        }
    }
}
