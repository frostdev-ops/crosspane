//! Mirror acceptance; only the explicitly selected nested compositor is addressed.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use crosspane_platform::{ParkingKind, WindowParking};
use crosspane_platform_linux::hyprland::{ipc::HyprIpc, mirror::HyprlandMirrorParking};
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use crosspane_types::id::{DisplayId, WindowId};
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    child: Child,
    journal: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.journal);
        let _ = std::fs::remove_file(self.journal.with_extension("tmp"));
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for fixture");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn clients(ipc: &HyprIpc) -> Vec<Value> {
    ipc.json("clients").unwrap().as_array().unwrap().clone()
}
fn borders(ipc: &HyprIpc, address: &str) -> [String; 2] {
    ["active_border_color", "inactive_border_color"].map(|prop| {
        ipc.request(&format!("getprop address:{address} {prop}"))
            .unwrap()
            .trim()
            .to_owned()
    })
}
fn count(path: &PathBuf) -> usize {
    serde_json::from_slice::<Vec<Value>>(&std::fs::read(path).unwrap())
        .unwrap()
        .len()
}

#[test]
fn mirror_lifecycle_and_crash_recovery() {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: requires explicit nested Hyprland");
        return;
    }
    let ipc = HyprIpc::from_env().unwrap();
    let title = format!("crosspane-mirror-{}", std::process::id());
    let binary =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/crosspane-testapp");
    let child = Command::new(binary)
        .args(["window", "--title", &title])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut fixture = Fixture {
        child,
        journal: std::env::temp_dir().join(format!("cp-mirror-{}.json", std::process::id())),
    };
    let _ = std::fs::remove_file(&fixture.journal);
    wait(|| clients(&ipc).iter().any(|c| c["title"] == title));
    let client = clients(&ipc)
        .into_iter()
        .find(|c| c["title"] == title)
        .unwrap();
    let window = WindowId(u64::from_str_radix(client["stableId"].as_str().unwrap(), 16).unwrap());
    let address = client["address"].as_str().unwrap();
    // Include pre-existing overrides with transparency, multiple colours, and nonzero angles.
    // The sentinel is required by the installed 0.56.2 multi-token setter.
    for (prop, value) in [
        (
            "active_border_color",
            "gradient 0x44112233 0x88556677 45deg",
        ),
        (
            "inactive_border_color",
            "gradient 0xff123456 0xffabcdef 90deg",
        ),
    ] {
        ipc.dispatch(&format!("hl.dsp.window.set_prop({{ window = \"address:{address}\", prop = \"{prop}\", value = \"{value}\" }})")).unwrap();
    }
    let before = borders(&ipc, address);
    assert_eq!(
        before,
        ["44112233 88556677 45deg", "ff123456 ffabcdef 90deg"]
    );
    let mut rejected =
        HyprlandMirrorParking::new(ipc.clone(), fixture.journal.clone(), "invalid-colour").unwrap();
    assert!(rejected.park(window, PixelSize::new(1, 1), 1.0).is_err());
    assert_eq!(borders(&ipc, address), before);
    assert!(!fixture.journal.exists());
    let mut parking =
        HyprlandMirrorParking::new(ipc.clone(), fixture.journal.clone(), "rgb(ff8800)").unwrap();
    let parked = parking.park(window, PixelSize::new(123, 456), 3.0).unwrap();
    assert_eq!(parked.kind, ParkingKind::Mirror);
    let current = clients(&ipc)
        .into_iter()
        .find(|c| c["stableId"] == client["stableId"])
        .unwrap();
    assert_eq!(current["at"], client["at"]);
    assert_eq!(current["size"], client["size"]);
    let monitor = ipc
        .json("monitors")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == current["monitor"])
        .unwrap()
        .clone();
    let scale = monitor["scale"].as_f64().unwrap();
    let x = ((current["at"][0].as_f64().unwrap() - monitor["x"].as_f64().unwrap()) * scale).round()
        as i32;
    let y = ((current["at"][1].as_f64().unwrap() - monitor["y"].as_f64().unwrap()) * scale).round()
        as i32;
    let w = (current["size"][0].as_f64().unwrap() * scale).round() as i32;
    let h = (current["size"][1].as_f64().unwrap() * scale).round() as i32;
    assert_eq!(
        parked.display,
        DisplayId(monitor["id"].as_u64().unwrap() as u32)
    );
    assert_eq!(
        parked.content,
        PixelRect::new(point2(x, y), point2(x + w, y + h))
    );
    assert_eq!(borders(&ipc, address), ["ffff8800 0deg", "ffff8800 0deg"]);
    assert_eq!(count(&fixture.journal), 1);
    assert_eq!(
        parking.resize(window, PixelSize::new(1, 1), 9.0).unwrap(),
        parking.geometry(window).unwrap()
    );
    parking.restore(window).unwrap();
    assert_eq!(borders(&ipc, address), before);
    assert_eq!(count(&fixture.journal), 0);
    parking.restore(window).unwrap();
    parking.park(window, PixelSize::new(1, 1), 1.0).unwrap();
    drop(parking);
    let mut recovery =
        HyprlandMirrorParking::new(ipc.clone(), fixture.journal.clone(), "rgb(00ff00)").unwrap();
    assert_eq!(recovery.recover().unwrap(), vec![window]);
    assert_eq!(borders(&ipc, address), before);
    assert_eq!(count(&fixture.journal), 0);
    assert!(recovery.recover().unwrap().is_empty());
    recovery.park(window, PixelSize::new(1, 1), 1.0).unwrap();
    fixture.child.kill().unwrap();
    fixture.child.wait().unwrap();
    wait(|| {
        !clients(&ipc)
            .iter()
            .any(|c| c["stableId"] == client["stableId"])
    });
    recovery.restore(window).unwrap();
    assert_eq!(count(&fixture.journal), 0);
}
