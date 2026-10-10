#![cfg(target_os = "linux")]
use std::process::Command;
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
