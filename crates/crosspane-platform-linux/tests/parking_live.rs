//! Twin-output parking against the **live** Hyprland session: headless outputs can't allocate in a
//! nested instance on this GPU (P1a), so this test is lead-run only. Skipped unless
//! `CROSSPANE_LIVE_HYPR=1`. It opens its own throwaway `foot` window, parks it, resizes it,
//! restores it, closes it, and checks the real monitors never moved.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use crosspane_platform::WindowParking;
use crosspane_platform_linux::hyprland::ipc::HyprIpc;
use crosspane_platform_linux::hyprland::parking::{HyprlandParking, OUTPUT_PREFIX};
use crosspane_types::geom::PixelSize;
use crosspane_types::id::WindowId;
use serde_json::Value;

fn monitors(ipc: &HyprIpc) -> Vec<(String, i64, i64, i64, i64)> {
    let mut list: Vec<_> = ipc
        .json("monitors")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["name"].as_str().unwrap().to_owned(),
                m["x"].as_i64().unwrap(),
                m["y"].as_i64().unwrap(),
                m["width"].as_i64().unwrap(),
                m["height"].as_i64().unwrap(),
            )
        })
        .collect();
    list.sort();
    list
}

fn client(ipc: &HyprIpc, class: &str) -> Option<Value> {
    ipc.json("clients")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["class"] == class)
        .cloned()
}

#[test]
fn park_resize_restore_live() {
    if std::env::var("CROSSPANE_LIVE_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: lead-run live test (CROSSPANE_LIVE_HYPR=1)");
        return;
    }
    let ipc = HyprIpc::from_env().unwrap();
    let before = monitors(&ipc);
    assert!(
        before.iter().all(|m| !m.0.starts_with(OUTPUT_PREFIX)),
        "leftover twin outputs"
    );

    let class = format!("crosspane-park-live-{}", std::process::id());
    ipc.dispatch(&format!(
        "hl.dsp.exec_cmd(\"foot --app-id {class} sleep 120\")"
    ))
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let c = loop {
        if let Some(c) = client(&ipc, &class) {
            break c;
        }
        assert!(Instant::now() < deadline, "test window didn't appear");
        std::thread::sleep(Duration::from_millis(50));
    };
    let window = WindowId(u64::from_str_radix(c["stableId"].as_str().unwrap(), 16).unwrap());
    let original_ws = c["workspace"]["name"].as_str().unwrap().to_owned();
    let address = c["address"].as_str().unwrap().to_owned();

    let journal = std::env::temp_dir().join(format!("cp-park-live-{}.json", std::process::id()));
    let mut parking = HyprlandParking::new(ipc.clone(), journal.clone()).unwrap();

    let t = Instant::now();
    let parked = parking
        .park(window, PixelSize::new(1600, 1200), 2.0)
        .unwrap();
    eprintln!("parked in {:?}: {parked:?}", t.elapsed());
    assert!(
        std::fs::read_to_string(&journal)
            .unwrap()
            .contains(&address)
    );
    assert_eq!(parked.content.width(), 1600);
    assert_eq!(parked.content.height(), 1200);
    let during = monitors(&ipc);
    let real: Vec<_> = during
        .iter()
        .filter(|m| !m.0.starts_with(OUTPUT_PREFIX))
        .cloned()
        .collect();
    assert_eq!(real, before, "a real monitor moved or changed");
    assert!(during.iter().any(|m| m.0.starts_with(OUTPUT_PREFIX)));

    let resized = parking
        .resize(window, PixelSize::new(1200, 900), 2.0)
        .unwrap();
    eprintln!("resized: {resized:?}");
    assert_eq!(
        (resized.content.width(), resized.content.height()),
        (1200, 900)
    );

    parking.restore(window).unwrap();
    let c = client(&ipc, &class).unwrap();
    assert_eq!(c["workspace"]["name"].as_str().unwrap(), original_ws);
    assert_eq!(
        monitors(&ipc),
        before,
        "twin output not removed or monitors changed"
    );
    assert!(std::fs::read_to_string(&journal).unwrap().trim() == "[]");

    // Crash recovery: park, forget the in-memory state, recover from the journal alone.
    let _ = parking.park(window, PixelSize::new(800, 600), 1.0).unwrap();
    drop(parking);
    let mut fresh = HyprlandParking::new(ipc.clone(), journal.clone()).unwrap();
    let restored = fresh.recover().unwrap();
    assert_eq!(restored, vec![window]);
    let c = client(&ipc, &class).unwrap();
    assert_eq!(c["workspace"]["name"].as_str().unwrap(), original_ws);
    assert_eq!(monitors(&ipc), before);

    ipc.dispatch(&format!(
        "hl.dsp.window.close({{ window = \"address:{address}\" }})"
    ))
    .unwrap();
    let _ = std::fs::remove_file(&journal);
}
