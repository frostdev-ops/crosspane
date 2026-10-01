//! IPC and `Displays` against a nested Hyprland (`scripts/hypr-nested.sh`). Skipped unless
//! `CROSSPANE_NESTED_HYPR=1`, which only the nested instance's env sets: the lead's shell points at
//! the live session, and tests must never depend on that.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use crosspane_platform::Displays;
use crosspane_platform_linux::hyprland::displays::HyprlandDisplays;
use crosspane_platform_linux::hyprland::ipc::{HyprIpc, IpcEvent};
use crosspane_types::display::DisplayInfo;

fn nested() -> Option<HyprIpc> {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: needs a nested Hyprland (eval \"$(scripts/hypr-nested.sh env)\")");
        return None;
    }
    Some(HyprIpc::from_env().unwrap())
}

#[test]
fn ipc_basics() {
    let Some(ipc) = nested() else { return };
    let v = ipc.require_supported().unwrap();
    assert!((v.major, v.minor) >= (0, 56));
    let monitors = ipc.json("monitors").unwrap();
    assert!(!monitors.as_array().unwrap().is_empty());
    assert!(!ipc.monitor_ids().unwrap().is_empty());
    ipc.eval("local crosspane_test = 1").unwrap();
    assert!(ipc.eval("this is not lua").is_err());
    assert!(
        ipc.request("definitely-not-a-command")
            .unwrap()
            .contains("unknown")
    );
}

#[test]
fn events_see_a_new_window() {
    let Some(ipc) = nested() else { return };
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let stream = ipc
        .events(Box::new(move |e| {
            if let IpcEvent::Event { name, data } = e {
                let _ = tx.lock().unwrap().send((name.to_owned(), data.to_owned()));
            }
        }))
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    ipc.dispatch(r#"hl.dsp.exec_cmd("foot --app-id crosspane-ipc-test")"#)
        .unwrap();
    let seen = (0..200).find_map(|_| {
        let (name, data) = rx.recv_timeout(Duration::from_secs(5)).ok()?;
        (name == "openwindow" && data.contains("crosspane-ipc-test")).then_some(data)
    });
    assert!(seen.is_some(), "no openwindow event for the test window");
    let address = seen.unwrap().split(',').next().unwrap().to_owned();
    ipc.dispatch(&format!(
        r#"hl.dsp.window.close({{ window = "address:0x{address}" }})"#
    ))
    .unwrap_or_else(|e| eprintln!("close failed (window left open): {e}"));
    drop(stream);
}

#[test]
fn displays_match_monitors() {
    let Some(ipc) = nested() else { return };
    let mut displays = HyprlandDisplays::new(ipc.clone()).unwrap();
    let snapshot = displays.displays().unwrap();
    let monitors = ipc.json("monitors").unwrap();
    assert_eq!(snapshot.len(), monitors.as_array().unwrap().len());
    for (d, m) in snapshot.iter().zip(monitors.as_array().unwrap()) {
        assert_eq!(u64::from(d.id.0), m["id"].as_u64().unwrap());
        assert_eq!(d.geometry.scale, m["scale"].as_f64().unwrap());
        assert_eq!(d.geometry.logical_origin.x, m["x"].as_f64().unwrap());
        assert!(d.geometry.is_valid());
    }

    let got: Arc<Mutex<Vec<Vec<DisplayInfo>>>> = Arc::default();
    let sink = got.clone();
    displays
        .subscribe(Arc::new(move |s: Vec<DisplayInfo>| {
            sink.lock().unwrap().push(s)
        }))
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let first = got.lock().unwrap().first().cloned().unwrap();
    assert_eq!(first, snapshot);
}
