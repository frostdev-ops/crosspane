//! Overlay acceptance tests. All compositor access and injected clicks require the explicit nest.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crosspane_platform::{
    Overlay, OverlayAnchor, OverlayEvent, OverlayHost, OverlayId, PlatformError, Rgb8,
};
use crosspane_platform_linux::hyprland::ipc::HyprIpc;
use crosspane_platform_linux::hyprland::overlay::HyprlandOverlay;
use crosspane_types::id::DisplayId;
use serde_json::Value;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

fn nested() -> Option<HyprIpc> {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: needs a nested Hyprland (eval \"$(scripts/hypr-nested.sh env)\")");
        return None;
    }
    Some(HyprIpc::from_env().unwrap())
}

fn description(display: DisplayId, anchor: OverlayAnchor) -> Overlay {
    Overlay {
        display,
        anchor,
        text: "Crosspane".into(),
        accent: Rgb8 {
            r: 0,
            g: 180,
            b: 255,
        },
    }
}

fn host() -> (HyprlandOverlay, mpsc::Receiver<OverlayEvent>) {
    let mut host = HyprlandOverlay::new().unwrap();
    let (send, events) = mpsc::channel();
    host.subscribe(Arc::new(move |event| {
        send.send(event).unwrap();
    }))
    .unwrap();
    (host, events)
}

fn layers(ipc: &HyprIpc, output: &str) -> Vec<Value> {
    let layers = ipc.json("layers").unwrap();
    layers[output]["levels"]["3"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|layer| layer["namespace"] == "crosspane-overlay")
        .cloned()
        .collect()
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for fixture/compositor"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn show_present_update_hide_and_unknown_display() {
    let Some(ipc) = nested() else { return };
    let (output, monitor) = ipc.monitor_ids().unwrap().remove(0);
    let (mut host, events) = host();
    let id = OverlayId(123);
    let mut overlay = description(DisplayId(monitor), OverlayAnchor::TopRight);
    let start = Instant::now();
    host.show(id, &overlay).unwrap();
    assert_eq!(
        events.recv_timeout(Duration::from_millis(200)).unwrap(),
        OverlayEvent::Visible(id)
    );
    assert!(start.elapsed() <= Duration::from_millis(200));
    let listed = layers(&ipc, &output);
    let original = listed.iter().find(|layer| layer["w"] == 172).unwrap();
    assert!(original["h"].as_u64().unwrap() > 0);
    let address = original["address"].clone();
    let unknown = Overlay {
        display: DisplayId(u32::MAX),
        ..overlay.clone()
    };
    assert!(matches!(
        host.show(OverlayId(999), &unknown),
        Err(PlatformError::NotFound)
    ));
    overlay.text = "Updated".into();
    host.show(id, &overlay).unwrap();
    assert_eq!(
        events.recv_timeout(Duration::from_millis(200)).unwrap(),
        OverlayEvent::Visible(id)
    );
    let updated = layers(&ipc, &output);
    assert_eq!(
        updated
            .iter()
            .find(|layer| layer["address"] == address)
            .unwrap()["w"],
        140
    );
    host.show(id, &overlay).unwrap();
    assert_eq!(
        events.recv_timeout(Duration::from_millis(200)).unwrap(),
        OverlayEvent::Visible(id)
    );
    host.hide(id).unwrap();
    wait_until(|| {
        layers(&ipc, &output)
            .iter()
            .all(|layer| layer["address"] != address)
    });
    host.hide(id).unwrap();
    assert!(events.try_recv().is_err());
}

struct Fixture {
    child: Child,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let executable = std::env::current_exe().unwrap();
        let binary = executable
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("crosspane-testapp");
        if !binary.is_file() {
            assert!(
                Command::new("cargo")
                    .args(["build", "--locked", "-p", "crosspane-testapp"])
                    .status()
                    .unwrap()
                    .success(),
                "build crosspane-testapp fixture"
            );
        }
        let log =
            std::env::temp_dir().join(format!("crosspane-overlay-{}.jsonl", std::process::id()));
        let child = Command::new(binary)
            .args(["window", "--title", "crosspane-overlay-fixture", "--events"])
            .arg(&log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let fixture = Self { child, log };
        wait_until(|| {
            fixture
                .events()
                .iter()
                .any(|event| event["event"] == "ready")
        });
        fixture
    }

    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.log);
    }
}

#[test]
fn virtual_pointer_click_passes_through_overlay_to_testapp() {
    let Some(ipc) = nested() else { return };
    let fixture = Fixture::new();
    let monitors = ipc.json("monitors").unwrap();
    let monitor = &monitors.as_array().unwrap()[0];
    let display = DisplayId(monitor["id"].as_u64().unwrap() as u32);
    let output_name = monitor["name"].as_str().unwrap();
    let clients = ipc.json("clients").unwrap();
    let client = clients
        .as_array()
        .unwrap()
        .iter()
        .find(|client| client["title"] == "crosspane-overlay-fixture")
        .unwrap();
    let address = client["address"].as_str().unwrap();
    // Fullscreen just this fixture in the nest, so the center overlay has a window underneath it.
    ipc.dispatch(&format!(
        r#"hl.dsp.focus({{ window = "address:{address}" }})"#
    ))
    .unwrap();
    ipc.dispatch("hl.dsp.window.fullscreen(0)").unwrap();
    wait_until(|| {
        let clients = ipc.json("clients").unwrap();
        clients
            .as_array()
            .unwrap()
            .iter()
            .any(|client| client["address"] == address && client["fullscreen"] == 2)
    });
    let active_before = ipc.json("activewindow").unwrap()["address"].clone();
    let (mut host, events) = host();
    let id = OverlayId(456);
    let mut overlay = description(display, OverlayAnchor::Center);
    overlay.text = "Click through".into();
    host.show(id, &overlay).unwrap();
    assert_eq!(
        events.recv_timeout(Duration::from_millis(200)).unwrap(),
        OverlayEvent::Visible(id)
    );
    assert_eq!(ipc.json("activewindow").unwrap()["address"], active_before);
    let listed = layers(&ipc, output_name);
    let width = monitor["width"].as_u64().unwrap() as u32;
    let height = monitor["height"].as_u64().unwrap() as u32;
    let layer = listed
        .iter()
        .find(|layer| (layer["y"].as_i64().unwrap() + 20 - i64::from(height / 2)).abs() < 3)
        .unwrap();
    let x = layer["x"].as_u64().unwrap() as u32 + layer["w"].as_u64().unwrap() as u32 / 2;
    let y = layer["y"].as_u64().unwrap() as u32 + layer["h"].as_u64().unwrap() as u32 / 2;
    let connection = Connection::connect_to_env().unwrap();
    let (globals, mut queue) = registry_queue_init::<PointerState>(&connection).unwrap();
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).unwrap();
    let output: wl_output::WlOutput = globals.bind(&qh, 4..=4, ()).unwrap();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 2..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer_with_output(Some(&seat), Some(&output), &qh, ());
    pointer.motion_absolute(0, 1, 1, width, height);
    pointer.frame();
    queue.roundtrip(&mut PointerState).unwrap();
    pointer.motion_absolute(0, x, y, width, height);
    pointer.frame();
    queue.roundtrip(&mut PointerState).unwrap();
    pointer.button(1, 272, wl_pointer::ButtonState::Pressed);
    pointer.frame();
    pointer.button(2, 272, wl_pointer::ButtonState::Released);
    pointer.frame();
    queue.roundtrip(&mut PointerState).unwrap();
    wait_until(|| {
        let events = fixture.events();
        ["pressed", "released"].iter().all(|state| {
            events.iter().any(|event| {
                event["event"] == "button" && event["button"] == 1 && event["state"] == *state
            })
        })
    });
    pointer.destroy();
    host.hide(id).unwrap();
}

fn nested_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/hypr-nested.sh")
}

struct DedicatedNest {
    name: String,
}

impl Drop for DedicatedNest {
    fn drop(&mut self) {
        let status = Command::new(nested_script())
            .args(["stop", "--name", &self.name])
            .status();
        if !status.is_ok_and(|status| status.success()) {
            eprintln!("failed to stop dedicated nested instance {}", self.name);
        }
    }
}

// Re-execute only this test with the dedicated nest's environment. Do not mutate the process
// environment (other tests may be running), and never send an exit to the shared acceptance nest.
fn dedicated(test: &str) -> Option<HyprIpc> {
    let ipc = nested()?;
    if std::env::var("CROSSPANE_OVERLAY_CHILD_TEST").as_deref() == Ok(test) {
        return Some(ipc);
    }
    let name = format!("wp-1-23-{}-{}", test, std::process::id());
    let nest = DedicatedNest { name };
    let parent = std::env::var("CROSSPANE_PARENT_WAYLAND_DISPLAY")
        .unwrap_or_else(|_| std::env::var("WAYLAND_DISPLAY").unwrap());
    assert!(
        Command::new(nested_script())
            .args(["start", "--name", &nest.name])
            .env("WAYLAND_DISPLAY", parent)
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("WAYLAND_SOCKET")
            .status()
            .unwrap()
            .success()
    );
    let result = Command::new(nested_script())
        .args(["env", "--name", &nest.name])
        .output()
        .unwrap();
    assert!(result.status.success());
    let exports = String::from_utf8(result.stdout).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", test, "--nocapture"])
        .env("CROSSPANE_OVERLAY_CHILD_TEST", test)
        .env("CROSSPANE_OVERLAY_NEST_NAME", &nest.name)
        .env_remove("WAYLAND_SOCKET");
    for line in exports.lines() {
        let (key, value) = line
            .strip_prefix("export ")
            .unwrap()
            .split_once('=')
            .unwrap();
        assert!(matches!(
            key,
            "WAYLAND_DISPLAY" | "HYPRLAND_INSTANCE_SIGNATURE" | "CROSSPANE_NESTED_HYPR"
        ));
        assert!(
            value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        );
        child.env(key, value);
    }
    let result = child.output().unwrap();
    assert!(
        result.status.success(),
        "dedicated test failed:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    None
}

struct ExtraOutput {
    ipc: HyprIpc,
    name: String,
    display: DisplayId,
}

impl ExtraOutput {
    fn new(ipc: &HyprIpc) -> Self {
        let before = ipc.monitor_ids().unwrap();
        assert_eq!(ipc.request("output create wayland").unwrap().trim(), "ok");
        let mut added = None;
        wait_until(|| {
            added = ipc
                .monitor_ids()
                .unwrap()
                .into_iter()
                .find(|output| !before.contains(output));
            added.is_some()
        });
        let (name, id) = added.unwrap();
        Self {
            ipc: ipc.clone(),
            name,
            display: DisplayId(id),
        }
    }
}

impl Drop for ExtraOutput {
    fn drop(&mut self) {
        let _ = self.ipc.request(&format!("output remove {}", self.name));
    }
}

#[test]
fn output_removal_reports_unavailable_and_not_found() {
    let Some(ipc) = dedicated("output_removal_reports_unavailable_and_not_found") else {
        return;
    };
    let (mut host, events) = host();
    let output = ExtraOutput::new(&ipc);
    let overlay = description(output.display, OverlayAnchor::TopCenter);
    let id = OverlayId(701);
    // The host was constructed before hotplug: its IPC helper must discover the new name.
    wait_until(|| match host.show(id, &overlay) {
        Ok(()) => true,
        Err(PlatformError::NotFound) => false,
        Err(error) => panic!("show on new output: {error}"),
    });
    assert_eq!(
        events.recv_timeout(Duration::from_millis(200)).unwrap(),
        OverlayEvent::Visible(id)
    );
    assert!(!layers(&ipc, &output.name).is_empty());
    let removed = output.display;
    drop(output);
    assert_eq!(
        events.recv_timeout(Duration::from_secs(2)).unwrap(),
        OverlayEvent::Unavailable(id)
    );
    assert!(matches!(
        host.show(id, &description(removed, OverlayAnchor::TopCenter)),
        Err(PlatformError::NotFound)
    ));
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
}

#[test]
fn move_to_another_display_emits_one_visible() {
    let Some(ipc) = dedicated("move_to_another_display_emits_one_visible") else {
        return;
    };
    let (first_name, first_id) = ipc.monitor_ids().unwrap().remove(0);
    let second = ExtraOutput::new(&ipc);
    let (mut host, events) = host();
    let id = OverlayId(702);
    let mut overlay = description(DisplayId(first_id), OverlayAnchor::TopRight);
    host.show(id, &overlay).unwrap();
    assert_eq!(
        events.recv_timeout(Duration::from_millis(200)).unwrap(),
        OverlayEvent::Visible(id)
    );
    let old = layers(&ipc, &first_name).remove(0)["address"].clone();
    overlay.display = second.display;
    host.show(id, &overlay).unwrap();
    let retained = layers(&ipc, &first_name)
        .iter()
        .any(|layer| layer["address"] == old);
    match events.try_recv() {
        Ok(event) => assert_eq!(event, OverlayEvent::Visible(id)),
        Err(mpsc::TryRecvError::Empty) => {
            assert!(
                retained,
                "old HUD disappeared before replacement was presented"
            );
            assert_eq!(
                events.recv_timeout(Duration::from_millis(200)).unwrap(),
                OverlayEvent::Visible(id)
            );
        }
        Err(error) => panic!("overlay events: {error}"),
    }
    wait_until(|| {
        layers(&ipc, &first_name)
            .iter()
            .all(|layer| layer["address"] != old)
    });
    assert_eq!(layers(&ipc, &second.name).len(), 1);
    assert!(
        events.recv_timeout(Duration::from_millis(550)).is_err(),
        "duplicate Visible after move"
    );
    drop(host);
    assert_eq!(
        events.recv_timeout(Duration::from_millis(200)).unwrap(),
        OverlayEvent::Unavailable(id)
    );
}

#[test]
fn compositor_exit_reports_unavailable() {
    let Some(ipc) = dedicated("compositor_exit_reports_unavailable") else {
        return;
    };
    let display = DisplayId(ipc.monitor_ids().unwrap().remove(0).1);
    let (mut host, events) = host();
    let ids = [OverlayId(703), OverlayId(704)];
    for id in ids {
        host.show(id, &description(display, OverlayAnchor::TopCenter))
            .unwrap();
        assert_eq!(
            events.recv_timeout(Duration::from_millis(200)).unwrap(),
            OverlayEvent::Visible(id)
        );
    }
    let name = std::env::var("CROSSPANE_OVERLAY_NEST_NAME").unwrap();
    assert!(
        Command::new(nested_script())
            .args(["stop", "--name", &name])
            .status()
            .unwrap()
            .success()
    );
    let mut lost: Vec<_> = (0..ids.len())
        .map(|_| events.recv_timeout(Duration::from_secs(2)).unwrap())
        .collect();
    lost.sort_by_key(|event| match event {
        OverlayEvent::Unavailable(id) | OverlayEvent::Visible(id) => *id,
    });
    assert_eq!(lost, ids.map(OverlayEvent::Unavailable));
    assert!(
        host.show(ids[0], &description(display, OverlayAnchor::TopCenter))
            .is_err()
    );
}

struct PointerState;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for PointerState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(PointerState: ignore wl_seat::WlSeat);
delegate_noop!(PointerState: ignore wl_output::WlOutput);
delegate_noop!(PointerState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(PointerState: ignore ZwlrVirtualPointerV1);
