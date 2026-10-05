#![cfg(target_os = "linux")]
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn real_check_accepts_notes_and_safety_stops_and_rejects_evidence_hard_stops() {
    let root = PathBuf::from(format!(
        "/tmp/cp429-diagnose-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let scratch = Scratch(root);
    let binary = scratch.0.join("installer");
    fs::write(
        &binary,
        b"#!/bin/sh\ncat \"$(dirname \"$0\")/report.json\"\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let payload = scratch.0.join("payload");
    fs::create_dir(&payload).unwrap();
    for stop in [false, true] {
        let mut facts = ["support", "payload", "service", "agent"].into_iter().map(|step| serde_json::json!({"step":step,"check":"authority", "class":"S", "value":null,"issue":"Unknown","hard_stop":true})).collect::<Vec<_>>();
        facts.push(serde_json::json!({"step":"support","check":"compatibility", "class":"E", "value":null,"issue":"Unavailable","hard_stop":stop}));
        fs::write(scratch.0.join("report.json"), serde_json::to_vec(&serde_json::json!({"schema_version":1,"platform":"linux","read_only":true,"facts":facts})).unwrap()).unwrap();
        let output = Command::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/installer/real-check.sh"
        ))
        .arg("--linux-only")
        .arg("--linux-installer")
        .arg(&binary)
        .arg("--linux-payload")
        .arg(&payload)
        .arg("--output")
        .arg(scratch.0.join("output"))
        .output()
        .unwrap();
        assert_eq!(
            output.status.success(),
            !stop,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("4 S hard stops"));
    }
}
#[test]
fn diagnose_without_a_selected_target_returns_json_without_starting_a_gui() {
    let output = Command::new(env!("CARGO_BIN_EXE_crosspane-installer"))
        .arg("--diagnose")
        .env_remove("HOME")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["read_only"], true);
    assert!(
        value["facts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["class"] == "S" && f["hard_stop"] == true)
    );
}
