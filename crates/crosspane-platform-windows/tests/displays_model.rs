use crosspane_platform::PlatformError;
use crosspane_platform_windows::model::{
    displays::{Delivery, NativeMonitor, TargetPath, commit, join_paths, read_snapshot},
    geometry::{DisplayIds, MonitorProbe},
};
use std::sync::Mutex;

fn monitor(path: &str, handle: usize, primary: bool) -> NativeMonitor {
    NativeMonitor {
        handle,
        probe: MonitorProbe {
            device_path: path.into(),
            name: format!("source-{path}"),
            rc_monitor: if primary {
                [0, 0, 1800, 1200]
            } else {
                [1800, 0, 4200, 1600]
            },
            rc_work: if primary {
                [0, 0, 1800, 1160]
            } else {
                [1800, 0, 4200, 1560]
            },
            primary,
            dpi: if primary { 96 } else { 144 },
            refresh_millihz: 60_000,
            edid: None,
            twin: false,
            quarter_turns: 0,
        },
    }
}

#[test]
fn retained_ids_survive_disconnect_and_handle_replacement() {
    let mut ids = DisplayIds::default();
    let a = commit(
        vec![monitor("a", 1, true), monitor("b", 2, false)],
        &mut ids,
    )
    .unwrap();
    let b_id = a.ids.clone().assign("b").unwrap();
    commit(vec![monitor("a", 1, true)], &mut ids).unwrap();
    let b = commit(
        vec![monitor("a", 1, true), monitor("b", 99, false)],
        &mut ids,
    )
    .unwrap();
    assert_eq!(b.ids.clone().assign("b").unwrap(), b_id);
    assert_eq!(b.monitors[&b_id], 99);
    assert!(!b.monitors.values().any(|handle| *handle == 2));
}

#[test]
fn native_identity_must_have_a_nonzero_unique_handle_and_path() {
    let mut ids = DisplayIds::default();
    assert!(commit(vec![monitor("a", 0, true)], &mut ids).is_err());
    assert_eq!(format!("{ids:?}"), format!("{:?}", DisplayIds::default()));
    assert!(
        commit(
            vec![monitor("a", 1, true), monitor("b", 1, false)],
            &mut ids
        )
        .is_err()
    );
    assert_eq!(format!("{ids:?}"), format!("{:?}", DisplayIds::default()));
    assert!(commit(vec![monitor("", 1, true)], &mut ids).is_err());
}

#[test]
fn topology_churn_refuses_without_allocating_or_using_stale_facts() {
    let ids = Mutex::new(DisplayIds::default());
    let mut calls = 0;
    let result = read_snapshot(
        &mut || {
            calls += 1;
            Ok(vec![monitor(
                if calls % 2 == 0 { "b" } else { "a" },
                calls,
                true,
            )])
        },
        &ids,
    );
    assert!(matches!(result, Err(PlatformError::Timeout)));
    assert_eq!(
        format!("{:?}", ids.lock().unwrap()),
        format!("{:?}", DisplayIds::default())
    );
    assert!(calls <= 4);
}

#[test]
fn refresh_is_coherent_and_native_calls_never_hold_allocator_mutex() {
    let ids = Mutex::new(DisplayIds::default());
    let first = read_snapshot(
        &mut || {
            assert!(ids.try_lock().is_ok());
            Ok(vec![monitor("a", 10, true)])
        },
        &ids,
    )
    .unwrap();
    let second = read_snapshot(
        &mut || {
            assert!(ids.try_lock().is_ok());
            Ok(vec![monitor("b", 20, true)])
        },
        &ids,
    )
    .unwrap();
    let new_id = second.ids.clone().assign("b").unwrap();
    assert_ne!(new_id, first.ids.clone().assign("a").unwrap());
    assert_eq!(second.monitors[&new_id], 20);
    assert_eq!(second.displays[0].id, new_id);
    assert_eq!(second.probes[0].device_path, "b");
}

#[test]
fn invalid_geometry_does_not_consume_ids() {
    let mut ids = DisplayIds::default();
    let mut bad = monitor("bad", 1, true);
    bad.probe.dpi = 0;
    assert!(commit(vec![bad], &mut ids).is_err());
    assert_eq!(format!("{ids:?}"), format!("{:?}", DisplayIds::default()));
}

fn target(source: &str) -> TargetPath {
    TargetPath {
        source_name: source.into(),
        device_path: format!("actual-target-{source}"),
        display_name: "actual-friendly-name".into(),
        refresh_numerator: 60_000,
        refresh_denominator: 1001,
        quarter_turns: 1,
    }
}

#[test]
fn source_to_target_join_preserves_actual_fields_without_fallback_identity() {
    let native = monitor("a", 1, true);
    let source = native.probe.name.clone();
    let observed = join_paths(vec![native], &[target(&source)]).unwrap();
    assert_eq!(
        observed[0].probe.device_path,
        format!("actual-target-{source}")
    );
    assert_eq!(observed[0].probe.name, source);
    assert_eq!(observed[0].probe.refresh_millihz, 59_940);
    assert_eq!(observed[0].probe.quarter_turns, 1);
    assert_eq!(observed[0].probe.edid, None);
    assert!(join_paths(vec![monitor("a", 1, true)], &[target("missing-source")]).is_err());
    let mut empty = target(&source);
    empty.device_path.clear();
    assert!(join_paths(vec![monitor("a", 1, true)], &[empty]).is_err());
}

#[test]
fn joined_probe_remains_usable_by_the_frozen_window_consumer() {
    let native = monitor("a", 1, true);
    let source_name = native.probe.name.clone();
    let bounds = native.probe.rc_monitor;
    let probes: Vec<_> = join_paths(vec![native], &[target(&source_name)])
        .unwrap()
        .into_iter()
        .map(|monitor| monitor.probe)
        .collect();
    let mut ids = DisplayIds::default();
    let result = crosspane_platform_windows::model::window::logical_frame(
        [10, 10, 210, 190],
        bounds,
        &source_name,
        &probes,
        &mut ids,
    );
    assert!(
        result.is_ok(),
        "native GDI source name must survive the CCD target join"
    );
}

#[test]
fn mirrored_or_invalid_target_reports_are_refused() {
    let source = monitor("a", 1, true).probe.name;
    assert!(
        join_paths(
            vec![monitor("a", 1, true)],
            &[target(&source), target(&source)]
        )
        .is_err()
    );
    let mut invalid = target(&source);
    invalid.refresh_denominator = 0;
    assert!(join_paths(vec![monitor("a", 1, true)], &[invalid]).is_err());
    let mut invalid = target(&source);
    invalid.quarter_turns = 4;
    assert!(join_paths(vec![monitor("a", 1, true)], &[invalid]).is_err());
}

#[test]
fn subscription_initial_then_loss_then_recovered_latest_cannot_skip_empty() {
    let mut ids = DisplayIds::default();
    let first = commit(vec![monitor("a", 1, true)], &mut ids)
        .unwrap()
        .displays;
    let latest = commit(vec![monitor("b", 2, true)], &mut ids)
        .unwrap()
        .displays;
    let mut delivery = Delivery::new(first.clone());
    delivery.publish(latest.clone());
    delivery.publish(Vec::new());
    for _ in 0..100 {
        delivery.publish(latest.clone());
    }
    assert_eq!(delivery.take(), Some(first));
    assert_eq!(delivery.take(), Some(Vec::new()));
    assert_eq!(delivery.take(), Some(latest));
    assert_eq!(delivery.take(), None);
}

#[test]
fn mixed_dpi_geometry_keeps_matching_allocator_and_handles() {
    let mut ids = DisplayIds::default();
    let snapshot = commit(
        vec![monitor("a", 1, true), monitor("b", 2, false)],
        &mut ids,
    )
    .unwrap();
    for display in &snapshot.displays {
        assert!(snapshot.monitors.contains_key(&display.id));
    }
    assert_eq!(snapshot.probes[1].dpi, 144);
    assert_eq!(snapshot.displays.len(), 2);
}
